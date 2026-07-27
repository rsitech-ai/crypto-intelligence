//! recovery::backup implementation boundary.
#[derive(Clone,Debug,Eq,PartialEq)] pub struct BackupContract { pub schema_version:u32, pub identifier:String }
impl BackupContract { pub fn new(identifier:impl Into<String>)->Result<Self,&'static str>{let identifier=identifier.into();if identifier.is_empty()||identifier.len()>256{return Err("invalid identifier")}Ok(Self{schema_version:1,identifier})} }
