//! H.264 for whole-screen motion: OpenH264 on computers. Phones have none yet (OpenH264 is
//! C++ and would need the C++ runtime in the app; they will use the system's hardware
//! encoder instead), so there the picture always goes as tiles.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use rdesk_common::h264::{H264Encoder, bitrate, usable};

#[cfg(any(target_os = "android", target_os = "ios"))]
mod none {
    use anyhow::{Result, bail};
    use rdesk_common::tiles::Frame;

    pub struct H264Encoder(std::convert::Infallible);

    pub fn bitrate(_: u32, _: u32, _: u8, _: u8) -> u32 {
        0
    }

    /// Never: the video pipeline keeps to tiles.
    pub fn usable(_: u32, _: u32) -> bool {
        false
    }

    impl H264Encoder {
        pub fn new(_: u32, _: u32, _: u8, _: u32) -> Result<Self> {
            bail!("no H.264 encoder on phones yet")
        }

        pub fn size(&self) -> (u32, u32) {
            match self.0 {}
        }

        pub fn force_key(&mut self) {
            match self.0 {}
        }

        pub fn encode(&mut self, _: &Frame) -> Result<Vec<u8>> {
            match self.0 {}
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub use none::{H264Encoder, bitrate, usable};
