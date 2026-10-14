use glimpse_plugin_sdk::{serve,Action,Hit};
fn main(){
 if let Err(error)=serve(&["search"],|request|{
    if request.op!="search"{return Ok(serde_json::Value::Null);}
    let query=request.args["query"].as_str().unwrap_or_default();
    let hits:Vec<Hit>=query.strip_prefix("length ").map(|text|{let count=text.chars().count().to_string();Hit{id:"count".into(),title:format!("{count} characters"),subtitle:"Unicode scalar values · copy".into(),action:Action::Copy{text:count}}}).into_iter().collect();
    Ok(serde_json::to_value(hits)?)
 }){eprintln!("{error:#}");std::process::exit(1);}
}
