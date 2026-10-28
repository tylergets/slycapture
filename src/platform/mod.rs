#[cfg(target_os = "linux")]
pub mod applet;
pub mod desktop;
pub mod dunst;
pub mod i3;
#[cfg(target_os = "linux")]
pub mod sni_reregister;
pub mod x11;
