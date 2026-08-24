#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Advice {
    Normal = 0,
    Random = 1,
    Sequential = 2,
    WillNeed = 3,
    DontNeed = 4,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UncheckedAdvice {
    Normal = 0,
    Random = 1,
    Sequential = 2,
    WillNeed = 3,
    DontNeed = 4,
}

impl UncheckedAdvice {
    pub fn new(val: i32) -> Self {
        match val {
            0 => UncheckedAdvice::Normal,
            1 => UncheckedAdvice::Random,
            2 => UncheckedAdvice::Sequential,
            3 => UncheckedAdvice::WillNeed,
            _ => UncheckedAdvice::DontNeed,
        }
    }
    pub fn from_advice(advice: Advice) -> Self {
        match advice {
            Advice::Normal => UncheckedAdvice::Normal,
            Advice::Random => UncheckedAdvice::Random,
            Advice::Sequential => UncheckedAdvice::Sequential,
            Advice::WillNeed => UncheckedAdvice::WillNeed,
            Advice::DontNeed => UncheckedAdvice::DontNeed,
        }
    }
}
