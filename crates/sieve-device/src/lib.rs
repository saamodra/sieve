//! Device setup and teardown: Android (adb), iOS Simulator (simctl and the
//! Mac system proxy), and physical iOS devices (profile, PAC, QR).

pub mod android;
pub mod ios_device;
pub mod ios_sim;
