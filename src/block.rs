use serde_derive::{
    Serialize,
    Deserialize,
};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Debug)]
pub struct Id {
    serial: u64,
}

impl Default for Id {
    fn default() -> Id {
        Id::init()
    }
}

impl Id {
    pub fn init() -> Id {
        Id {
            serial: 0,
        }
    }

    pub fn next(&self) -> Id {
        Id {
            serial: self.serial + 1,
        }
    }
}

pub fn crc(bytes: &[u8]) -> u64 {
    crc::crc64::checksum_ecma(bytes)
}
