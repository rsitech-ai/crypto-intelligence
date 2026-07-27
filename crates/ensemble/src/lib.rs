//! ensemble production contract.
pub mod ablation;
pub mod constraints;
pub mod module_output;
pub mod oof_matrix;
pub mod stacker;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
