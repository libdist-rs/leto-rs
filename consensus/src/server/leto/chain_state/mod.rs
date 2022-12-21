mod core;
pub use self::core::*;

mod parent;
pub use parent::*;

mod batch;
pub use batch::*;

mod db;
pub use db::*;

use crypto::hash::Hash;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DBData<Data> {
    pub(super) key: Hash<Data>,
    pub(super) value: Data,
}
