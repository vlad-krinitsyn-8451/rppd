/*%LPH%*/


mod gen;

use std::str::FromStr;
use std::sync::Arc;
use once_cell::sync::Lazy;
use pgrx::*;
use pgrx::prelude::*;
use pgrx::WhoAllocated;
use tokio::runtime::{Builder, Runtime};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use crate::gen::rg::{DbAction, EventRequest, EventResponse, PkColumn, PkColumnType};
use crate::gen::rg::grpc_client::GrpcClient;
use crate::gen::rg::pk_column::PkValue::{BigintValue, IntValue};

pgrx::pg_module_magic!();

pub const CONFIG_TABLE: &str = "rppd_config";
pub const TIMEOUT_MS: u64 = 100;


extension_sql_file!("../pg_setup.sql", requires = [rppd_event] );

/// config and conenction
pub(crate) static CONFIG: Lazy<MetaConfig> = Lazy::new(|| {

	let runtime = Builder::new_multi_thread()
		.worker_threads(1)
		.thread_name("rd")
		.thread_stack_size(3 * 1024 * 1024)
		.enable_all()
		.build().map_err(|e| e.to_string()).unwrap();

	MetaConfig { server_path: Arc::new(RwLock::new("".into())),
		server: Arc::new(RwLock::new(Err("Error_Not_Yet_Connected".into()))),
		runtime: Arc::new(Mutex::new(runtime)) }
});

pub struct MetaConfig {
	/// path to connect
	pub server_path: Arc<RwLock<String>>,
	pub server: Arc<RwLock<Result<tokio::sync::Mutex<GrpcClient<Channel>>, String>>>,
	pub runtime: Arc<Mutex<Runtime>>,
}

impl MetaConfig {
	#[inline]
	fn needs_re_connect(&self, host: &str) -> Option<String> {
		match self.server_path.read() {
			Ok(path) => {
				if host == path.as_str() {
					match self.server.read() {
						Ok(s) => match &*s {
							Ok(_s) => None,
							Err(e) => Some(e.clone()),
						}
						Err(e) => Some(e.to_string()),
					}
				} else {
					Some(format!("connected to {}, required {}", path, host))
				}
			}
			Err(e) => Some(e.to_string())
		}
	}
}

#[pg_trigger]
fn rppd_event<'a>(
	trigger: &'a pgrx::PgTrigger<'a>,
) -> Result<Option<PgHeapTuple<'a, impl WhoAllocated>>, PgTriggerError> {
	let current =
		if trigger.event().fired_by_delete() {
			trigger.old().ok_or(PgTriggerError::NotTrigger)?//.into_owned()
		} else {
			trigger.new().ok_or(PgTriggerError::NotTrigger)?//.into_owned()
		};

	let mut pks = vec![];
	if let Ok(v) = current.get_by_name::<i32>("id") {
		pks.push(PkColumn{column_name: "id".to_string(), column_type: 0, pk_value: v.map(|v| IntValue(v)) });
	}

	let table_name = trigger.table_name().unwrap_or("".into());
	if table_name.as_str() == CONFIG_TABLE
		&& current.get_by_name::<bool>("master")
			.map_err(|_e| PgTriggerError::NullTriggerData)?.unwrap_or(false) {
		let host = current.get_by_name::<String>("host")
			.map_err(|_e| PgTriggerError::NotTrigger)?.unwrap_or("".into());
		if let Some(e) = CONFIG.needs_re_connect(host.as_str()) {
			if e.len() > 0 {
				pgrx::debug1!("connecting to: {}", host);
			} else {
				pgrx::notice!("re-connecting to: {}, previously: {}", host, e);
			}

			let client = {
				let path = host.clone();
				match CONFIG.runtime.lock() {
					Ok(runtime) => {
						runtime.block_on(async move {
							GrpcClient::connect(Duration::from_millis(TIMEOUT_MS),
												Endpoint::from_str(path.as_str())
													.map_err(|e| e.to_string())?)
								.await.map_err(|e| format!("connecting {}", e))
						})
					}
					Err(e) => {
						pgrx::notice!("connecting error. please restarted a session");
						Err(e.to_string())
					}
				}
			};

			{
				let mut path = CONFIG.server_path.write().unwrap();
				*path = host.clone();
			}
			let mut server = CONFIG.server.write().unwrap();
			*server = match client {
				Ok(the_client) => Ok(tokio::sync::Mutex::new(the_client)),
				Err(e) => {
					pgrx::warning!("error on connect to: {} : {}", host, e);
					Err(format!("server={}, {}", host, e))
				}
			};
		}
	}
	let event_type = if trigger.event().fired_by_update() {
		DbAction::Update as i32
	} else if trigger.event().fired_by_insert() {
		DbAction::Insert as i32
	} else if trigger.event().fired_by_delete() {
		DbAction::Delete as i32
	} else {
		DbAction::Truncate as i32
	};

	let table_name = format!("{}.{}", trigger.table_schema().unwrap_or("public".into()), table_name);
	let event = EventRequest {table_name, event_type, id_value: false, pks: pks.clone(), optional_caller: None};

	match call(event.clone()) {
		Ok(event_response) => {
			for column in &event_response.repeat_with {

				let pk_value = match column.column_type {
					1 => {
						assert_eq!((PkColumnType::BigInt as i32), 1);
						current.get_by_name::<i64>(column.column_name.as_str()).unwrap_or(None)
							.map(|v| BigintValue(v))
					}
					_ => current.get_by_name::<i32>(column.column_name.as_str()).unwrap_or(None)
						.map(|v| IntValue(v))
				};
				pks.push(PkColumn {pk_value, ..column.clone() });
			}
			if event_response.repeat_with.len() > 0 {
				let _ = call(EventRequest{pks, id_value: true, ..event});
			}

			Ok(Some(current))
		}
		Err(e) => {
			let host = CONFIG.server_path.read().unwrap().clone();
			let mut server = CONFIG.server.write().unwrap();
			*server = Err(e.clone());
			pgrx::warning!("error on event to {}: {} (will try to connect on next call) ", host, e);
			Err(PgTriggerError::NullTriggerData)
		}
	}
}

fn call(event: EventRequest) -> Result<EventResponse, String> {
	let client = CONFIG.server.clone();
	let runtime = CONFIG.runtime.lock().unwrap();
	runtime.block_on(async move {
		let client = client.read().unwrap();
		match &*client {
			Err(e) => Err(e.to_string()),
			Ok(the_client) => {
				let mut client = the_client.lock().await;
				// DO a call:
				client.event(tonic::Request::new(event)).await
					.map(|response| response.into_inner())
					.map_err(|e| e.message().to_string())
			}
		}
	})
}


#[cfg(any(test, feature = "pg_test"))]
#[pg_trigger]
fn trigger_example<'a>(
	trigger: &'a pgrx::PgTrigger<'a>,
) -> Result<Option<PgHeapTuple<'a, impl WhoAllocated>>, PgTriggerError> {
	let current =
		if trigger.event().fired_by_delete() {
			trigger.old().ok_or(PgTriggerError::NotTrigger)?
		} else {
			trigger.new().ok_or(PgTriggerError::NotTrigger)?
		};

	let id = current.get_by_name::<i64>("id")
		.map_err(|_e| PgTriggerError::NotTrigger)?.unwrap_or(0);
	let title = current.get_by_name::<String>("title")
		.map_err(|_e| PgTriggerError::NotTrigger)?.unwrap_or("".into());
	println!("OUT> {} = {} by {}  {}.{}", id, title, trigger.name().unwrap_or("NA"),
		trigger.table_schema().unwrap_or("".into()), trigger.table_name().unwrap_or("".into())
	);

	Ok(Some(current))
}



#[cfg(any(test, feature = "pg_test"))]

extension_sql!(
    r#"
CREATE TABLE ti (
    id bigserial NOT NULL PRIMARY KEY,
    data text,
    tx bigint
);

CREATE TABLE test (
    id bigserial NOT NULL PRIMARY KEY,
    title varchar(50),
    description text,
    payload jsonb default '{}',
    payload_n jsonb,
    flag bool default true,
    flag_n bool,
    df date default current_date,
    df_n date,
    dtf timestamp default current_timestamp,
    uf uuid default '672124b6-9894-11e5-be38-002d42e813fe',
    uf_n uuid,
    nrf numrange,
    tsrf tsrange
);

CREATE TRIGGER test_trigger AFTER INSERT OR UPDATE OR DELETE ON test FOR EACH ROW EXECUTE PROCEDURE trigger_example();
INSERT INTO test (title, description, payload) VALUES ('Fox', 'a description', '{"key": "value"}');
"#,
    name = "create_trigger",
    requires = [trigger_example]
);


#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
	use super::*;

	#[pg_test]
	fn test_insert() {
		println!("\nTEST\n");

		let _ = Spi::run(r#"INSERT INTO test (title, description, payload)
        VALUES ('a different title', 'a different description', '{"key": "value"}');
        "#, );

		println!("\n");

		let _ = Spi::run(r#"INSERT INTO test (title) VALUES ('update');"#, );

		println!("\nupdating:");
		let _ = Spi::run(r#"update test set description = 'a different description updated', title = 'tbd' where title= 'update';"#, );

		println!("\ndeleting:");
		let _ = Spi::run(r#"delete from test where title= 'tbd';"#, );

		assert!(true);
	}
}

#[cfg(test)]
pub mod pg_test {
	pub fn setup(_options: Vec<&str>) {
		println!("// perform one-off initialization when the pg_test framework starts");
	}

	pub fn postgresql_conf_options() -> Vec<&'static str> {
		// return any postgresql.conf settings that are required for your tests
		// use in, but no longer available: pgrx::pg_sys::GetConfigOptionByName();
		vec![
			"yt.test = 1111"
		]
	}

	#[test]
	pub fn test_pg_compile() {
		assert!(true)
	}


}
