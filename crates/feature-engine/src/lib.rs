//! feature-engine production contract.
pub mod clock;
pub mod features;
pub mod lineage;
pub mod materialize;
pub mod watermark;
pub mod window;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
