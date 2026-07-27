//! feature-registry::definition implementation boundary.
#[derive(Clone,Debug,Eq,PartialEq)] pub struct DefinitionContract { pub schema_version:u32, pub identifier:String }
impl DefinitionContract { pub fn new(identifier:impl Into<String>)->Result<Self,&'static str>{let identifier=identifier.into();if identifier.is_empty()||identifier.len()>256{return Err("invalid identifier")}Ok(Self{schema_version:1,identifier})} }
