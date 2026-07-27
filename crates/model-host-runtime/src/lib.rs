//! model-host-runtime production contract.
pub mod circuit_breaker;
pub mod parity;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
