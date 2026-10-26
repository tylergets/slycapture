pub mod audio;
pub mod encoder;
pub mod options;
pub mod recorder;
pub mod region;
pub mod transcode;

pub use options::{RecordingOptions, Target};
pub use recorder::Recording;
pub use region::Region;
