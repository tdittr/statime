use core::{
    cell::{Ref, RefCell},
    future::Future,
    pin::Pin,
};

use embassy_futures::{select, select::Either3};
pub use error::{PortError, Result};
use futures::StreamExt;
pub use measurement::Measurement;
use state::{MasterState, PortState};
pub use ticker::Ticker;

use self::state::SlaveState;
use crate::{
    bmc::bmca::{BestAnnounceMessage, Bmca, RecommendedState},
    clock::Clock,
    config::PortConfig,
    datastructures::{
        common::{PortIdentity, TimeSource, WireTimestamp},
        datasets::{CurrentDS, ParentDS, TimePropertiesDS},
        messages::Message,
    },
    filters::Filter,
    network::{NetworkPort, NetworkRuntime},
    ptp_instance::PtpInstanceState,
    time::Duration,
    utils::Signal,
    Time, MAX_DATA_LEN,
};

mod error;
mod measurement;
mod sequence_id;
pub mod state;
mod ticker;

/// A single port of the PTP instance
///
/// One of these needs to be created per port of the PTP instance.
pub struct Port<L> {
    config: PortConfig,
    // Corresponds with PortDS port_state and enabled
    port_state: PortState,
    bmca: Bmca,
    lifecycle: L,
}

// Temporary, hopefully gone after refactor
pub struct Startup<P> {
    network_port: P,
}

pub struct Running<'a, C, F> {
    state_refcell: &'a RefCell<PtpInstanceState<C, F>>,
    state: Ref<'a, PtpInstanceState<C, F>>,
}

pub struct InBmca<'a, C, F> {
    pending_action: Option<PortAction<'static>>,
    local_best: Option<BestAnnounceMessage>,
    state_refcell: &'a RefCell<PtpInstanceState<C, F>>,
}

// START NEW INTERFACE

// Making this non-copy and non-clone ensures a single handle_send_timestamp
// per SendTimeCritical
#[derive(Debug)]
pub struct TimestampContext {
    inner: TimestampContextInner,
}

#[derive(Debug)]
enum TimestampContextInner {
    DelayReq { id: u16 },
}

#[derive(Debug)]
pub enum PortAction<'a> {
    SendTimeCritical {
        context: TimestampContext,
        data: &'a [u8],
    },
    SendGeneral {
        data: &'a [u8],
    },
    ResetAnnounceTimer {
        duration: core::time::Duration,
    },
    ResetSyncTimer {
        duration: core::time::Duration,
    },
    ResetAnnounceReceiptTimer {
        duration: core::time::Duration,
    },
}

pub struct PortActionIterator<'a> {
    internal: <Option<PortAction<'a>> as IntoIterator>::IntoIter,
}

impl<'a> PortActionIterator<'a> {
    fn from(option: Option<PortAction<'a>>) -> Self {
        Self {
            internal: option.into_iter(),
        }
    }
}

impl<'a> Iterator for PortActionIterator<'a> {
    type Item = PortAction<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal.next()
    }
}

impl<'a, C: Clock, F: Filter> Port<Running<'a, C, F>> {
    // Send timestamp for last timecritical message became available
    pub fn handle_send_timestamp(
        &mut self,
        context: TimestampContext,
        timestamp: Time,
    ) -> PortActionIterator<'_> {
        PortActionIterator::from(self.port_state.handle_timestamp(context, timestamp))
    }

    // Handle the announce timer going of
    pub fn handle_announce_timer(&mut self) -> PortActionIterator<'_> {
        todo!();
        #[allow(unreachable_code)]
        PortActionIterator::from(None)
    }

    // Handle the sync timer going of
    pub fn handle_sync_timer(&mut self) -> PortActionIterator<'_> {
        todo!();
        #[allow(unreachable_code)]
        PortActionIterator::from(None)
    }

    // Handle the announce receipt timer going of
    pub fn handle_announce_receipt_timer(&mut self) -> PortActionIterator<'_> {
        todo!();
        #[allow(unreachable_code)]
        PortActionIterator::from(None)
    }

    // Handle a message over the timecritical channel
    pub fn handle_timecritical_receive<'b>(
        &mut self,
        data: &[u8],
        timestamp: Time,
        // Temporary parameters until refactoring of central state management
        buffer: &'b mut [u8],
    ) -> PortActionIterator<'b> {
        let message = match Message::deserialize(data) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("Could not parse packet: {:?}", error);
                return PortActionIterator::from(None);
            }
        };

        // Only process messages from the same domain
        if message.header().sdo_id() != self.lifecycle.state.default_ds.sdo_id
            || message.header().domain_number() != self.lifecycle.state.default_ds.domain_number
        {
            return PortActionIterator::from(None);
        }

        let actions = PortActionIterator::from(self.port_state.handle_event_receive(
            message,
            timestamp,
            self.config.min_delay_req_interval(),
            self.config.port_identity,
            &self.lifecycle.state.default_ds,
            buffer,
        ));

        if let Err(error) = self.temp_handle_time_measurement() {
            log::error!("Could not handle time measurement: {:?}", error);
        }

        actions
    }

    // Handle a general ptp message
    pub fn handle_general_receive<'b>(&mut self, data: &[u8]) -> PortActionIterator<'b> {
        let message = match Message::deserialize(data) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("Could not parse packet: {:?}", error);
                return PortActionIterator::from(None);
            }
        };

        // Only process messages from the same domain
        if message.header().sdo_id() != self.lifecycle.state.default_ds.sdo_id
            || message.header().domain_number() != self.lifecycle.state.default_ds.domain_number
        {
            return PortActionIterator::from(None);
        }

        let action = match message {
            Message::Announce(announce) => {
                self.bmca.register_announce_message(
                    &announce,
                    self.lifecycle.state.local_clock.borrow().now().into(),
                );
                Some(PortAction::ResetAnnounceReceiptTimer {
                    duration: core::time::Duration::from_secs_f64(
                        2f64.powi(self.config.log_announce_interval as i32)
                            * (self.config.announce_receipt_timeout as f64),
                    ),
                })
            }
            _ => {
                self.port_state
                    .handle_general_receive(message, self.config.port_identity);
                None
            }
        };

        if let Err(error) = self.temp_handle_time_measurement() {
            log::error!("Could not handle time measurement: {:?}", error);
        }

        PortActionIterator::from(action)
    }

    // Start a BMCA cycle and ensure this happens instantly from the perspective of
    // the port
    pub fn start_bmca(self) -> Port<InBmca<'a, C, F>> {
        Port {
            port_state: self.port_state,
            config: self.config,
            bmca: self.bmca,
            lifecycle: InBmca {
                pending_action: None,
                local_best: None,
                state_refcell: self.lifecycle.state_refcell,
            },
        }
    }
}

impl<'a, C, F> Port<InBmca<'a, C, F>> {
    // End a BMCA cycle and make the port available again
    pub fn end_bmca(
        self,
    ) -> (
        Port<Running<'a, C, F>>,
        impl Iterator<Item = PortAction<'static>>,
    ) {
        (
            Port {
                port_state: self.port_state,
                config: self.config,
                bmca: self.bmca,
                lifecycle: Running {
                    state_refcell: self.lifecycle.state_refcell,
                    state: self.lifecycle.state_refcell.borrow(),
                },
            },
            PortActionIterator::from(self.lifecycle.pending_action),
        )
    }
}
// END NEW INTERFACE

impl<L> Port<L> {
    fn set_forced_port_state(&mut self, state: PortState) {
        log::info!(
            "new state for port {}: {} -> {}",
            self.config.port_identity.port_number,
            self.port_state,
            state
        );
        self.port_state = state;
    }

    pub(crate) fn state(&self) -> &PortState {
        &self.port_state
    }

    pub(crate) fn number(&self) -> u16 {
        self.config.port_identity.port_number
    }

    // From here, functions are kept temporarily to make conversion easier
    pub(crate) fn announce_interval(&self) -> Duration {
        Duration::from_log_interval(self.config.log_announce_interval)
    }

    pub(crate) fn sync_interval(&self) -> Duration {
        Duration::from_log_interval(self.config.log_sync_interval)
    }

    pub(crate) fn announce_receipt_interval(&self) -> Duration {
        Duration::from_log_interval(self.config.log_announce_interval)
            * self.config.announce_receipt_timeout
    }

    pub(crate) fn identity(&self) -> PortIdentity {
        self.config.port_identity
    }
}

impl<'a, C, F> Port<InBmca<'a, C, F>> {
    pub(crate) fn calculate_best_local_announce_message(&mut self, current_time: WireTimestamp) {
        self.lifecycle.local_best = self.bmca.take_best_port_announce_message(current_time)
    }

    pub(crate) fn best_local_announce_message(&self) -> Option<BestAnnounceMessage> {
        self.lifecycle.local_best
    }

    pub(crate) fn set_recommended_state(
        &mut self,
        recommended_state: RecommendedState,
        time_properties_ds: &mut TimePropertiesDS,
        current_ds: &mut CurrentDS,
        parent_ds: &mut ParentDS,
    ) -> Result<()> {
        self.set_recommended_port_state(&recommended_state);

        match recommended_state {
            RecommendedState::M1(defaultds) | RecommendedState::M2(defaultds) => {
                current_ds.steps_removed = 0;
                current_ds.offset_from_master = Duration::ZERO;
                current_ds.mean_delay = Duration::ZERO;

                parent_ds.parent_port_identity.clock_identity = defaultds.clock_identity;
                parent_ds.parent_port_identity.port_number = 0;
                parent_ds.grandmaster_identity = defaultds.clock_identity;
                parent_ds.grandmaster_clock_quality = defaultds.clock_quality;
                parent_ds.grandmaster_priority_1 = defaultds.priority_1;
                parent_ds.grandmaster_priority_2 = defaultds.priority_2;

                time_properties_ds.leap59 = false;
                time_properties_ds.leap61 = false;
                time_properties_ds.current_utc_offset = 37;
                time_properties_ds.current_utc_offset_valid = false;
                time_properties_ds.ptp_timescale = true;
                time_properties_ds.time_traceable = false;
                time_properties_ds.frequency_traceable = false;
                time_properties_ds.time_source = TimeSource::InternalOscillator;
            }
            RecommendedState::M3(_) | RecommendedState::P1(_) | RecommendedState::P2(_) => {}
            RecommendedState::S1(announce_message) => {
                current_ds.steps_removed = announce_message.steps_removed() + 1;

                parent_ds.parent_port_identity = announce_message.header().source_port_identity();
                parent_ds.grandmaster_identity = announce_message.grandmaster_identity();
                parent_ds.grandmaster_clock_quality = announce_message.grandmaster_clock_quality();
                parent_ds.grandmaster_priority_1 = announce_message.grandmaster_priority_1();
                parent_ds.grandmaster_priority_2 = announce_message.grandmaster_priority_2();

                *time_properties_ds = announce_message.time_properties();
            }
        }

        // TODO: Discuss if we should change the clock's own time properties, or keep
        // the master's time properties separately
        if let RecommendedState::S1(announce_message) = &recommended_state {
            // Update time properties
            *time_properties_ds = announce_message.time_properties();
        }

        Ok(())
    }

    fn set_recommended_port_state(&mut self, recommended_state: &RecommendedState) {
        match recommended_state {
            // TODO set things like steps_removed once they are added
            // TODO make sure states are complete
            RecommendedState::S1(announce_message) => {
                let remote_master = announce_message.header().source_port_identity();
                let state = PortState::Slave(SlaveState::new(remote_master));

                match &self.port_state {
                    PortState::Listening | PortState::Master(_) | PortState::Passive => {
                        self.set_forced_port_state(state);
                        self.lifecycle.pending_action =
                            Some(PortAction::ResetAnnounceReceiptTimer {
                                duration: core::time::Duration::from_secs_f64(
                                    2f64.powi(self.config.log_announce_interval as i32)
                                        * (self.config.announce_receipt_timeout as f64),
                                ),
                            });
                    }
                    PortState::Slave(old_state) => {
                        if old_state.remote_master() != remote_master {
                            self.set_forced_port_state(state);
                            self.lifecycle.pending_action =
                                Some(PortAction::ResetAnnounceReceiptTimer {
                                    duration: core::time::Duration::from_secs_f64(
                                        2f64.powi(self.config.log_announce_interval as i32)
                                            * (self.config.announce_receipt_timeout as f64),
                                    ),
                                });
                        }
                    }
                }
            }
            RecommendedState::M1(_) | RecommendedState::M2(_) | RecommendedState::M3(_) => {
                match self.port_state {
                    PortState::Listening | PortState::Slave(_) | PortState::Passive => {
                        self.set_forced_port_state(PortState::Master(MasterState::new()))
                    }
                    PortState::Master(_) => (),
                }
            }
            RecommendedState::P1(_) | RecommendedState::P2(_) => match self.port_state {
                PortState::Listening | PortState::Slave(_) | PortState::Master(_) => {
                    self.set_forced_port_state(PortState::Passive)
                }
                PortState::Passive => (),
            },
        }
    }
}

impl<P> Port<Startup<P>> {
    /// Create a new port from a port dataset on a given interface.
    ///
    /// For example, when using the `statime-linux` network runtime, a port on
    /// `eth0` for an ordinary clock can be created with
    ///
    /// ```ignore
    /// let port_ds = PortDS::new(
    ///     PortIdentity {
    ///         clock_identity,
    ///         port_number: 1,
    ///     },
    ///     1,
    ///     1,
    ///     3,
    ///     0,
    ///     DelayMechanism::E2E,
    ///     1,
    /// );
    /// let port = Port::new(port_ds, &mut network_runtime, "eth0".parse().unwrap());
    /// ```
    pub async fn new<NR>(
        config: PortConfig,
        runtime: &mut NR,
        interface: NR::InterfaceDescriptor,
    ) -> Self
    where
        NR: NetworkRuntime<NetworkPort = P>,
    {
        let network_port = runtime
            .open(interface)
            .await
            .expect("Could not create network port");

        let bmca = Bmca::new(
            Duration::from_log_interval(config.log_announce_interval).into(),
            config.port_identity,
        );

        Port {
            config,
            port_state: PortState::Listening,
            bmca,
            lifecycle: Startup { network_port },
        }
    }

    pub(crate) fn into_running<C, F>(
        self,
        state_refcell: &RefCell<PtpInstanceState<C, F>>,
    ) -> (Port<Running<'_, C, F>>, P) {
        (
            Port {
                config: self.config,
                port_state: self.port_state,
                bmca: self.bmca,
                lifecycle: Running {
                    state_refcell,
                    state: state_refcell.borrow(),
                },
            },
            self.lifecycle.network_port,
        )
    }
}

impl<'a, C: Clock, F: Filter> Port<Running<'a, C, F>> {
    fn temp_handle_time_measurement(&mut self) -> Result<()> {
        // If the received message allowed the (slave) state to calculate its offset
        // from the master, update the local clock
        if let Some(measurement) = self.port_state.extract_measurement() {
            let (offset, freq_corr) = self
                .lifecycle
                .state
                .filter
                .try_borrow_mut()
                .map(|mut borrow| borrow.absorb(measurement))
                .map_err(|_| PortError::FilterBusy)?;

            let mut local_clock = self
                .lifecycle
                .state
                .local_clock
                .try_borrow_mut()
                .map_err(|_| PortError::ClockBusy)?;

            if let Err(error) =
                local_clock.adjust(offset, freq_corr, &self.lifecycle.state.time_properties_ds)
            {
                log::error!("failed to adjust clock: {:?}", error);
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_port<FT: Future, P: NetworkPort>(
        &mut self,
        network_port: &mut P,
        announce_receipt_timeout: &mut Pin<&mut Ticker<FT, impl FnMut(Duration) -> FT>>,
        sync_timeout: &mut Pin<&mut Ticker<FT, impl FnMut(Duration) -> FT>>,
        announce_timeout: &mut Pin<&mut Ticker<FT, impl FnMut(Duration) -> FT>>,
        mut stop: Signal<'_>,
    ) {
        loop {
            log::trace!("Loop iter port {}", self.config.port_identity.port_number);
            let timeouts = select::select3(
                announce_receipt_timeout.next(),
                sync_timeout.next(),
                announce_timeout.next(),
            );
            let packet = network_port.recv();
            match select::select3(timeouts, packet, stop.wait_for()).await {
                Either3::First(timeout) => match timeout {
                    Either3::First(_) => {
                        log::trace!(
                            "Port {} force master timeout",
                            self.config.port_identity.port_number
                        );
                        // No announces received for a long time, become master
                        match self.port_state {
                            PortState::Master(_) => (),
                            _ => self.set_forced_port_state(PortState::Master(MasterState::new())),
                        }
                    }
                    Either3::Second(_) => {
                        log::trace!(
                            "Port {} sync timeout",
                            self.config.port_identity.port_number
                        );
                        // Send sync message
                        if let Err(error) = self.send_sync(network_port).await {
                            log::error!("{:?}", error);
                        }
                    }
                    Either3::Third(_) => {
                        log::trace!(
                            "Port {} announce timeout",
                            self.config.port_identity.port_number
                        );
                        // Send announce message
                        if let Err(error) = self.send_announce(network_port).await {
                            log::error!("{:?}", error);
                        }
                    }
                },
                Either3::Second(Ok(packet)) => {
                    log::trace!(
                        "Port {} message received: {:?}",
                        self.config.port_identity.port_number,
                        packet
                    );
                    let mut buffer = [0u8; MAX_DATA_LEN];
                    let actions = match packet.timestamp {
                        Some(timestamp) => {
                            self.handle_timecritical_receive(&packet.data, timestamp, &mut buffer)
                        }
                        None => self.handle_general_receive(&packet.data),
                    };
                    self.temp_handle_actions(actions, network_port, announce_receipt_timeout)
                        .await;
                }
                Either3::Second(Err(error)) => log::error!("failed to parse packet {:?}", error),
                Either3::Third(_) => {
                    log::trace!(
                        "Port {} bmca trigger",
                        self.config.port_identity.port_number
                    );
                    break;
                }
            }
        }
    }

    async fn temp_handle_actions<FT: Future, P: NetworkPort>(
        &mut self,
        actions: PortActionIterator<'_>,
        network_port: &mut P,
        announce_receipt_timeout: &mut Pin<&mut Ticker<FT, impl FnMut(Duration) -> FT>>,
    ) {
        for action in actions {
            match action {
                PortAction::SendTimeCritical { context, data } => {
                    match network_port.send_time_critical(data).await {
                        Ok(Some(timestamp)) => {
                            let mut followup = self.handle_send_timestamp(context, timestamp);
                            assert!(followup.next().is_none());
                        }
                        Ok(None) => {
                            log::error!("Missing timestamp for packet");
                        }
                        Err(error) => {
                            log::error!("Could not send message: {:?}", error)
                        }
                    }
                }
                PortAction::ResetAnnounceReceiptTimer { .. } => announce_receipt_timeout.reset(),
                _ => panic!("Unexpected action here"),
            }
        }
    }

    #[allow(clippy::await_holding_refcell_ref)]
    async fn send_sync<P: NetworkPort>(&mut self, network_port: &mut P) -> Result<()> {
        self.port_state
            .send_sync(
                &self.lifecycle.state.local_clock,
                network_port,
                self.config.port_identity,
                &self.lifecycle.state.default_ds,
            )
            .await
    }

    #[allow(clippy::await_holding_refcell_ref)]
    async fn send_announce<P: NetworkPort>(&mut self, network_port: &mut P) -> Result<()> {
        self.port_state
            .send_announce(
                &self.lifecycle.state.local_clock,
                &self.lifecycle.state.default_ds,
                &self.lifecycle.state.time_properties_ds,
                &self.lifecycle.state.parent_ds,
                &self.lifecycle.state.current_ds,
                network_port,
                self.config.port_identity,
            )
            .await
    }
}
