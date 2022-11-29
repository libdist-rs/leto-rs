use crate::Round;

#[derive(Debug)]
pub struct RoundContext {
    current_round: Round,
}

impl Default for RoundContext {
    fn default() -> Self {
        Self { current_round: Round::START }
    }
}

impl RoundContext {
    pub fn round(&self) -> Round {
        self.current_round
    }
}