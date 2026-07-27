//! chain-ethereum production contract.
pub mod consensus;
pub mod execution;
pub mod features;
pub mod stablecoins;
pub mod staking;
pub mod sync;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
