use crate::datastructures::{WireFormat, WireFormatError};
use core::fmt::Display;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct ClockIdentity(pub [u8; 8]);

impl WireFormat for ClockIdentity {
    fn wire_size(&self) -> usize {
        8
    }

    fn serialize(&self, buffer: &mut [u8]) -> Result<(), WireFormatError> {
        buffer[0..8].copy_from_slice(&self.0);
        Ok(())
    }

    fn deserialize(buffer: &[u8]) -> Result<Self, WireFormatError> {
        Ok(Self(buffer[0..8].try_into().unwrap()))
    }
}

impl Display for ClockIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_wireformat() {
        let representations = [(
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08u8],
            ClockIdentity([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]),
        )];

        for (byte_representation, object_representation) in representations {
            // Test the serialization output
            let mut serialization_buffer = [0; 8];
            object_representation
                .serialize(&mut serialization_buffer)
                .unwrap();
            assert_eq!(serialization_buffer, byte_representation);

            // Test the deserialization output
            let deserialized_data = ClockIdentity::deserialize(&byte_representation).unwrap();
            assert_eq!(deserialized_data, object_representation);
        }
    }

    #[test]
    fn format() {
        assert_eq!(
            ClockIdentity([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]).to_string(),
            "0102030405060708"
        )
    }
}
