use futures::{executor::block_on, stream::StreamExt};
use paho_mqtt as mqtt;
use reqwest::Response;
use select::document::{self, Document};
use select::predicate::{Any, Attr, Class, Name, Predicate, Text};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::error;
use std::fmt;
use std::{env, process, time::Duration};
use prometheus::{Opts, Registry, Counter, TextEncoder, Encoder, Gauge, labels, GaugeVec};

// Stat seems to mean status, tele I have no idea and cmnd is command
// Once a topic is subscribed to, you need to publish to the relevant "cmnd"
// topic in order to get updates for it.
const PREFIX: &[&str] = &["stat", "tele", "cmnd"];

// The topics to which we subscribe.
const TOPICS: &[&str] = &["machineinfo", "status", "config", "brewing/state"];

enum HeatingState {
    Undefined,
    Off,
    Heating,
    Break,
    Blocked,
}

enum PumpState {
    Undefined,
    Off,
    Pumping, // TODO: double check states
    On,
}

#[derive(Deserialize, Debug)]
struct MachineInfo {
    firmware: String,
    frequency: String,
    heating: u32,
    machineid: String,
    machinetype: String,
    timezone: String,
    voltage: String,
    volume: u8,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")] 
struct BrewingState {
    current_progress: u32,
    remaining_time: u32,
    total_progress: u32,
    version: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")] // for some reason the status data is returned in camelCase
struct Status {
    device_state: String,
    full_tilt_information: Option<Vec<String>>,
    heating: u8,
    pump: u8,
    pump_speed: Option<f64>,
    sensor_pressure: Option<f64>,
    target_temperature: Option<f64>,
    temperature: Option<f64>,
    version: String,
}

#[derive(Deserialize, Debug)]
struct Config {
    language: String,
    offset_to_utc: i32,
    temperature_unit: String,
    timezone: String,
    version: String,
    wort_unit: String,
}

struct SpeidelClient {
    username: String,
    password: String,
    machines: Machines,
    http_client: reqwest::blocking::Client,
}

#[derive(Debug, Clone)]
struct Machine {
    api_token: Option<String>,
    name: String,
    status: Option<Status>,
    temperature_gauge: Option<Gauge>,
}


struct Machines(HashMap<u64, Machine>);

type Result<T> = std::result::Result<T, Box<dyn error::Error>>;

#[derive(Debug, Clone)]
struct LoginError;

impl error::Error for LoginError {}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Login failed")
    }
}

#[derive(Debug, Clone)]
struct UnknownError;

impl error::Error for UnknownError {}

impl fmt::Display for UnknownError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "An unknown error occurred")
    }
}


impl Machine {
    fn new(name: String) -> Machine {
	Machine { api_token: None, name, status: None, temperature_gauge: None }
    }

    fn update(&mut self, status: Status) {
	self.status = Some(status);
    }
	
}

impl Machines {
    fn new() -> Machines {
        Machines {
            0: HashMap::<u64, Machine>::new(),
        }
    }
    fn add_machine(&mut self, id: u64, name: String) {
        self.0.insert(
            id, Machine::new(name)
        );
    }

    fn build(&mut self, client: reqwest::blocking::Client) -> Result<()> {
        for (id, machine) in self.0.iter_mut() {
            let resp = client.get(format!(
                    "https://www.myspeidel.com/braumeister/control/{}",
                    id.to_string()
                ))
                .send()?
                .text()?;

	       let doc = Document::from(resp.as_str());
            let bmv2control_data: Vec<Value> = doc
                .find(Name("script").descendant(Any))
                .filter(|script| script.text().contains("bmv2controlData"))
                .map(|script| {
                    serde_json::from_str(
                        script
                            .text()
                            .trim()
                            .strip_prefix("var bmv2controlData=")
                            .expect("bmv2controlData is in an unexpected format")
                            .strip_suffix(';')
                            .expect("bmv2controlData is in an unexpected format")
                    ).expect("unable to unmarshal bmv2controlData")
                })
                .collect();

            for config in bmv2control_data.iter() {
                machine.api_token = Some(config["apiAuthToken"]
                    .to_string()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect::<String>());
        }
    }
        Ok(())
    }

    fn from_resp(&mut self, index: reqwest::blocking::Response) -> Result<()> {
        match index.url().path() {
            "/auth/login" => return Err(LoginError.into()),
            "/myspeidel/index" => {
                let page = Document::from(index.text()?.as_str());

                for machine in page.find(Class("device-list").descendant(Class("teaser-box-item")))
                {
                    let machine_id: u64 = machine
                        .attr("data-machine-id")
                        .ok_or::<UnknownError>(UnknownError.into())?
                        .parse::<u64>()?;

                    let machine_name = machine
                        .attr("data-machine-name")
                        .ok_or::<UnknownError>(UnknownError.into())?;

                    self.add_machine(machine_id, machine_name.to_string());
                    println!("added {machine_name} ({machine_id})");
                }
                Ok(())
            },
            _ => Err(UnknownError.into())
        }
    }
}
impl BrewingState {
    fn new(value: &str) -> Result<BrewingState> {
	let result: BrewingState = serde_json::from_str(value)?;
	Ok(result)
    }
}

impl Status {
    fn new(value: &str) -> Result<Status> {
	let result: Status = serde_json::from_str(value)?;
	Ok(result)
    }
}



impl SpeidelClient {
    fn new(username: String, password: String) -> Result<SpeidelClient> {
        let client = match reqwest::blocking::ClientBuilder::new()
            .cookie_store(true)
            .gzip(true)
            .build()
        {
            Ok(client) => client,
            Err(error) => return Err(error.into()),
        };

        Ok(SpeidelClient {
            username,
            password,
            machines: Machines(HashMap::new()),
            http_client: client,
        })
    }

    fn login(&mut self) -> Result<()> {
        let params = [("identity", &self.username), ("password", &self.password)];


        self.http_client
            .post("https://www.myspeidel.com/auth/login")
            .form(&params)
            .send()
            .unwrap();

        let index = self
            .http_client
            .get("https://www.myspeidel.com/myspeidel/index")
            .send()
            .unwrap();

        match index.url().path() {
            "/auth/login" => return Err(LoginError.into()),
            "/myspeidel/index" => {
                self.machines.from_resp(index).expect("unable to retrieve machines");
                self.machines.build(self.http_client.clone()).expect("unable to retrieve API token");
		return Ok(());
            },
            _ => return Err(LoginError.into()),
        };
    }
}
fn machine_id_from_topic(topic: &str) -> Result<u64> {
    let s = match topic.split('/').into_iter().nth(1) {
	None => return Err(UnknownError.into()),
	Some(p) => p,
    };

    match s.parse::<u64>() {
	Err(e) => Err(e.into()),
	Ok(id) => Ok(id),
    }
}

fn main() {
    // Initialize the logger from the environment
    env_logger::init();


    let temperature_opts = Opts::new("temperature_gauge", "temperature of the brew kettle");
    let temperature_gauges = GaugeVec::new(temperature_opts, &["id", "name"]).unwrap();

    let target_temperature_opts = Opts::new("target_temperature_gauge", "target temperature of the brew kettle");
    let target_temperature_gauges = GaugeVec::new(target_temperature_opts, &["id", "name"]).unwrap();

    let device_info_opts = Opts::new("device_info", "information about machines");
    let device_info = GaugeVec::new(device_info_opts, &["firmware", "frequency", "heating", "id", "type", "timezone", "voltage", "volume"]).unwrap();

    let r = Registry::new();
    r.register(Box::new(temperature_gauges.clone())).unwrap();
    r.register(Box::new(target_temperature_gauges.clone())).unwrap();
    r.register(Box::new(device_info.clone())).unwrap();

    let host = env::args()
        .nth(1)
        .unwrap_or_else(|| "wss://api.cloud.myspeidel.com:443".to_string());

    let username = env::var("SPEIDEL_USERNAME").unwrap_or_else(|e| {
        println!(
            "The SPEIDEL_USERNAME environment variable must be provided: {:?}",
            e
        );
        process::exit(1);
    });

    let password = env::var("SPEIDEL_PASSWORD").unwrap_or_else(|e| {
        println!(
            "The SPEIDEL_PASSWORD environment variable must be provided: {:?}",
            e
        );
        process::exit(1);
    });

    let mut s = SpeidelClient::new(username, password).unwrap_or_else(|e| {
        println!("Unable to build the Speidel client: {:?}", e);
        process::exit(1);
    });

    s.login().unwrap_or_else(|e| {
        println!("Unable to login to MySpeidel: {:?}", e);
        process::exit(1);
    });

    let mut machine_id = String::new();
    let mut api_token = String::new();
    for (k, v) in s.machines.0.iter() {
        machine_id = k.to_string();
        api_token = v.api_token.as_ref().expect("API token missing").to_string();
    }

    let mut topics = vec![];
    let mut commands = vec![];

    for &prefix in PREFIX {
        for &topic in TOPICS {
            if prefix == "cmnd" {
                commands.push(format!("{}/{}/{}", prefix, machine_id, topic));
            } else {
                topics.push(format!("{}/{}/{}", prefix, machine_id, topic));
            }
        }
    }

    let qos = vec![0_i32; topics.len()];

    print!("{:?}", topics);

    // Create the client. Use an ID for a persistent session.
    // A real system should try harder to use a unique ID.
    let create_opts = mqtt::CreateOptionsBuilder::new()
        .server_uri(host)
        .client_id("brau-exporter")
        .finalize();

    // Create the client connection
    let mut cli = mqtt::AsyncClient::new(create_opts).unwrap_or_else(|e| {
        println!("Error creating the client: {:?}", e);
        process::exit(1);
    });

    if let Err(err) = block_on(async {
        // Get message stream before connecting.
        let mut strm = cli.get_stream(25);

        // Define the set of options for the connection
        let conn_opts = mqtt::ConnectOptionsBuilder::new()
            .keep_alive_interval(Duration::from_secs(30))
            .user_name(api_token)
            .password("")
            .ssl_options(mqtt::SslOptionsBuilder::new().finalize())
            .clean_session(false)
            .finalize();

        // Make the connection to the broker
        println!("Connecting to the MQTT server...");
        cli.connect(conn_opts).await?;

        println!("Subscribing to topics: {:?}", TOPICS);
        cli.subscribe_many(&topics, qos.as_slice()).await?;

        println!("Publishing request to receieve updates ({:?})...", commands);
        for command in commands {
            let msg = mqtt::Message::new(command, "", mqtt::QOS_1);
            cli.publish(msg).await?;
        }

        // Just loop on incoming messages.
        println!("Waiting for messages...");
        // Note that we're not providing a way to cleanly shut down and
        // disconnect. Therefore, when you kill this app (with a ^C or
        // whatever) the server will get an unexpected drop and then
        // should emit the LWT message.

        while let Some(msg_opt) = strm.next().await {
            if let Some(msg) = msg_opt {
                let payload = msg.payload_str().into_owned();

                if payload.clone().as_str().eq("") {
                    println!("{}: recieved empty message", msg.topic());
                    continue;
                }

                let p: Value = match serde_json::from_str(payload.as_str()) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("Unable to unmarshal message: {:?}", e);
                        continue;
                    }
                };

		let topic = match p["topic"].as_str() {
		    Some(v) => v,
		    None => {
			println!("Unable to find topic in message");
			continue;
		    }
		};

		let body = match Some(p["body"].to_string()) {
		    Some(v) => v,
		    None => {
			println!("Unable to find body in message");
			continue;
		    }
		};


		let machine_id = match machine_id_from_topic(msg.topic().to_string().as_str()) {
		    Ok(v) => v,
		    Err(e) => {
			println!("Unable to find machine id in topic {topic}: {:?}", e); 
			continue;
		    }
		};


                match topic {
                    "braumeister/machineinfo" => {
                        let m: MachineInfo =
                            serde_json::from_str(body.as_str()).unwrap();

			match s.machines.0.get(&machine_id) {
			    Some(_) => {
				let id = machine_id.to_string();
				let heating = m.heating.to_string();
				let volume = m.volume.to_string();
				device_info.with(&labels!{
				    "id" => id.as_str(),
				    "firmware" => m.firmware.as_str(),
				    "timezone" => m.timezone.as_str(),
				    "frequency" => m.frequency.as_str(),
				    "heating" => heating.as_str(),
				    "type" => m.machinetype.as_str(),
				    "voltage" => m.voltage.as_str(),
				    "volume" => volume.as_str(),
				    
				}).set(1.0);
			    },
			    None => {
				println!("machine not found!");
				continue;
			    }
			}
                        println!("{:?}", m);
                    }
                    "braumeister/status" => {
			let m = Status::new(body.as_str()).unwrap();
			match s.machines.0.get(&machine_id) {
			    Some(v) => {
				let id = machine_id.to_string();
				let labels = labels!{
				    "id" => id.as_str(),
				    "name" => v.name.as_str(),
				};
				temperature_gauges.with(&labels).set(m.temperature.unwrap());
				target_temperature_gauges.with(&labels).set(m.target_temperature.unwrap());
			    },
			    None => {
				println!("machine not found!");
				continue;
			    },
			}
			
			
                        println!("{:?}", m);
                    }
                    "braumeister/config" => {
                        let m: Config =
                            serde_json::from_str(body.as_str()).unwrap();
                        println!("{:?}", m);
                    }
		    "braumeister/brewing/state" => {
			let m = BrewingState::new(body.as_str()).unwrap();
			println!("{:?}", m);
		    }
                    unknown => {
                        println!("received unknown topic: {}", unknown);
                    }
                }

		let mut buffer = vec![];
		let encoder = TextEncoder::new();
		let metric_families = r.gather();
		encoder.encode(&metric_families, &mut buffer).unwrap();

		// Output to the standard output.
		println!("{}", String::from_utf8(buffer).unwrap());
            } else {
                // A "None" means we were disconnected. Try to reconnect...
                println!("Lost connection. Attempting reconnect.");
                while let Err(err) = cli.reconnect().await {
                    println!("Error reconnecting: {}", err);
                    // For tokio use: tokio::time::delay_for()
                    async_std::task::sleep(Duration::from_millis(1000)).await;
                }
            }
        }

        // Explicit return type for the async block
        Ok::<(), mqtt::Error>(())
    }) {
        eprintln!("{}", err);
    }
}
