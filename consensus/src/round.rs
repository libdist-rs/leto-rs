use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone)]
pub struct Round(usize);

impl Round {
    pub const START: Self = Self(0);
}

impl Default for Round {
    fn default() -> Self {
        Self(10)
    }
}

impl From<usize> for Round {
    fn from(n: usize) -> Self {
        Self(n)
    }
}

impl Debug for Round {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Display for Round {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Sub<Self> for Round {
    type Output = Self;

    fn sub(
        self,
        rhs: Self,
    ) -> Self::Output {
        Self(self.0.sub(rhs.0))
    }
}

impl std::ops::Add for Round {
    type Output = Self;

    fn add(
        self,
        rhs: Self,
    ) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl std::ops::Mul for Round {
    type Output = Round;

    fn mul(self, rhs: Self) -> Self::Output {
        Self(self.0 * rhs.0)
    }
}

impl std::ops::AddAssign for Round {
    fn add_assign(
        &mut self,
        rhs: Self,
    ) {
        self.0 += rhs.0;
    }
}

impl mempool::Round for Round {
    const MIN: Self = Self(0);
}

impl network::Message for Round {}
