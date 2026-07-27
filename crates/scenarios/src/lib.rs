//! scenarios production contract.
pub mod liquidity;
pub mod process;
pub mod summary;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
