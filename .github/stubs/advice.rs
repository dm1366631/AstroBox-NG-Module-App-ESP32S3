#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Advice {
    Normal = 0,
    Random = 1,
    Sequential = 2,
    WillNeed = 3,
    DontNeed = 4,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct UncheckedAdvice(pub i32);

impl UncheckedAdvice {
    pub const fn new(val: i32) -> Self { Self(val) }
    pub const fn from_advice(advice: Advice) -> Self { Self(advice as i32) }
}
