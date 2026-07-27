//! local-api production contract.
pub mod alert_service;
pub mod auth;
pub mod compatibility;
pub mod cusp_service;
pub mod forecast_service;
pub mod generated;
pub mod model_host_client;
pub mod stream_buffer;

#[derive(Clone,Copy,Debug,Eq,PartialEq)] pub struct ContractMetadata { pub schema_version:u32, pub bounded:bool, pub point_in_time:bool }
impl Default for ContractMetadata { fn default()->Self{Self{schema_version:1,bounded:true,point_in_time:true}} }
