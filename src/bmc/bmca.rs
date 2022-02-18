use super::{
    dataset_comparison::{ComparisonDataset, DatasetOrdering, DefaultDS},
    foreign_master::ForeignMasterList,
    PortState,
};
use crate::{
    datastructures::{
        common::{PortIdentity, TimeInterval, Timestamp},
        messages::AnnounceMessage,
    },
    time::{OffsetTime, TimeType},
};

/// Object implementing the Best Master Clock Algorithm
///
/// Usage:
///
/// - Every port has its own instance.
/// - When a port receives an announce message, it has to register it with the [Bmca::register_announce_message] method
/// - When it is time to run the algorithm, the ptp runtime has to take all the best announce messages using [Bmca::take_best_port_announce_message]
/// - Of the resulting set, the best global one needs to be determined. This can be done using [Bmca::find_best_announce_message]
/// - Then to get the recommended state for each port, [Bmca::calculate_recommended_state] needs to be called
pub struct Bmca {
    foreign_master_list: ForeignMasterList,
    own_port_identity: PortIdentity,
}

impl Bmca {
    pub fn new(own_port_announce_interval: TimeInterval, own_port_identity: PortIdentity) -> Self {
        Self {
            foreign_master_list: ForeignMasterList::new(
                own_port_announce_interval,
                own_port_identity,
            ),
            own_port_identity,
        }
    }

    /// Register a received announce message to the BMC algorithm
    pub fn register_announce_message(
        &mut self,
        announce_message: &AnnounceMessage,
        current_time: Timestamp,
    ) {
        self.foreign_master_list
            .register_announce_message(announce_message, current_time);
    }

    /// Takes the Erbest from this port. If called before two announce intervals have passed since the last call
    /// the returned value will like to be None. So don't do that.
    pub fn take_best_port_announce_message(
        &mut self,
        current_time: Timestamp,
        port_state: PortState,
    ) -> Option<(AnnounceMessage, PortIdentity)> {
        let announce_messages = self
            .foreign_master_list
            .take_qualified_announce_messages(current_time)
            .map(|fm| {
                let (best_announce_message, best_timestamp) = fm.get_best_announce_message();
                (
                    best_announce_message,
                    best_timestamp,
                    self.own_port_identity,
                )
            });

        let erbest = Self::find_best_announce_message(announce_messages);

        if let Some((erbest, erbest_ts, _)) = erbest {
            // If the port is in the slave, uncalibrated or passive state, then the best announce message must be reconsidered the next time
            if matches!(
                port_state,
                PortState::Slave | PortState::Uncalibrated | PortState::Passive
            ) {
                self.register_announce_message(&erbest, erbest_ts);
            }
        }

        erbest.map(|erbest| (erbest.0, erbest.2))
    }

    /// Finds the best announce message in the given iterator.
    /// The port identity in the tuple is the identity of the port that received the announce message.
    pub fn find_best_announce_message(
        announce_messages: impl Iterator<Item = (AnnounceMessage, Timestamp, PortIdentity)>,
    ) -> Option<(AnnounceMessage, Timestamp, PortIdentity)> {
        announce_messages.reduce(|(l, lts, lpid), (r, rts, rpid)| {
            match ComparisonDataset::from_announce_message(&l, &lpid)
                .compare(&ComparisonDataset::from_announce_message(&r, &rpid))
            {
                DatasetOrdering::Better | DatasetOrdering::BetterByTopology => (l, lts, lpid),
                // We get errors if two announce messages are (functionally) the same, in that case we just pick the newer one
                DatasetOrdering::Error1 | DatasetOrdering::Error2 => {
                    if OffsetTime::from_timestamp(&lts) >= OffsetTime::from_timestamp(&rts) {
                        (l, lts, lpid)
                    } else {
                        (r, rts, rpid)
                    }
                }
                DatasetOrdering::WorseByTopology | DatasetOrdering::Worse => (r, rts, rpid),
            }
        })
    }

    /// Calculates the recommended port state. This has to be run for every port.
    /// The PTP spec calls this the State Decision Algorithm.
    ///
    /// - `own_data`: Called 'D0' by the PTP spec. The DefaultDS data of our own ptp instance.
    /// - `best_global_announce_message`: Called 'Ebest' by the PTP spec. This is the best announce message and the
    /// identity of the port that received it of all of the best port announce messages.
    /// - `best_port_announce_message`: Called 'Erbest' by the PTP spec. This is the best announce message and the
    /// identity of the port that received it of the port we are calculating the recommended state for.
    /// - `port_state`: The current state of the port we are doing the calculation for.
    ///
    /// If None is returned, then the port should remain in the same state as it is now.
    pub fn calculate_recommended_state(
        own_data: &DefaultDS,
        best_global_announce_message: Option<(&AnnounceMessage, &PortIdentity)>,
        best_port_announce_message: Option<(&AnnounceMessage, &PortIdentity)>,
        port_state: PortState,
    ) -> Option<RecommendedState> {
        let d0 = ComparisonDataset::from_own_data(own_data);
        let ebest = best_global_announce_message
            .map(|(announce, pid)| ComparisonDataset::from_announce_message(announce, pid));
        let erbest = best_port_announce_message
            .map(|(announce, pid)| ComparisonDataset::from_announce_message(announce, pid));

        if best_global_announce_message.is_none() && matches!(port_state, PortState::Listening) {
            return None;
        }

        if (1..=127).contains(&own_data.clock_quality.clock_class) {
            return match erbest {
                None => Some(RecommendedState::M1(*own_data)),
                Some(erbest) => {
                    if d0.compare(&erbest).is_better() {
                        Some(RecommendedState::M1(*own_data))
                    } else {
                        Some(RecommendedState::P1(*best_port_announce_message.unwrap().0))
                    }
                }
            };
        }

        match &ebest {
            None => return Some(RecommendedState::M2(*own_data)),
            Some(ebest) => {
                if d0.compare(ebest).is_better() {
                    return Some(RecommendedState::M2(*own_data));
                }
            }
        }

        // If ebest was empty, then we would have returned in the previous step
        let best_global_announce_message = best_global_announce_message.unwrap();
        let ebest = ebest.unwrap();

        match erbest {
            None => Some(RecommendedState::M3(*best_global_announce_message.0)),
            Some(erbest) => {
                let best_port_announce_message = best_port_announce_message.unwrap();

                if best_global_announce_message.1 == best_port_announce_message.1 {
                    Some(RecommendedState::S1(*best_global_announce_message.0))
                } else if matches!(ebest.compare(&erbest), DatasetOrdering::BetterByTopology) {
                    Some(RecommendedState::P2(*best_port_announce_message.0))
                } else {
                    Some(RecommendedState::M3(*best_global_announce_message.0))
                }
            }
        }
    }
}

pub enum RecommendedState {
    M1(DefaultDS),
    M2(DefaultDS),
    M3(AnnounceMessage),
    P1(AnnounceMessage),
    P2(AnnounceMessage),
    S1(AnnounceMessage),
}
