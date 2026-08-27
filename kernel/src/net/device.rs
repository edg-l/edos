#[expect(
    dead_code,
    reason = "the device trait in full; e1000e is the only implementor so far"
)]
pub trait NetDevice: Send {
    fn send(&mut self, data: &[u8]) -> Result<(), &'static str>;
    fn mac_address(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
}
