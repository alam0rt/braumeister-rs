use futures::{executor::block_on, stream::StreamExt};
use html_parser::{Dom, Result};
use paho_mqtt as mqtt;
use select::document::{self, Document};
use select::predicate::{Any, Attr, Class, Name, Predicate, Text};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::{env, error::Error, process, time::Duration};

// Stat seems to mean status, tele I have no idea and cmnd is command
// Once a topic is subscribed to, you need to publish to the relevant "cmnd"
// topic in order to get updates for it.
const PREFIX: &[&str] = &["stat", "tele", "cmnd"];

// The topics to which we subscribe.
const TOPICS: &[&str] = &["machineinfo", "status", "config"];

#[derive(Deserialize, Debug)]
struct MachineInfo {
    firmware: String,
    frequency: String,
    heatings: u32,
    machineid: String,
    machinetype: String,
    timezone: String,
    voltage: String,
    volume: u8,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")] // for some reason the status data is returned in camelCase
struct Status {
    device_state: String,
    full_tilt_information: Vec<String>,
    heating: u8,
    pump: u8,
    pump_speed: Option<f32>,
    sensor_pressure: Option<f32>,
    target_temperature: Option<f32>,
    temperature: Option<f32>,
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
    machines: HashMap<u64, Machine>,
    http_client: reqwest::blocking::Client,
}

struct Machine {
    api_token: String,
}

impl SpeidelClient {
    fn new(username: String, password: String) -> Result<SpeidelClient> {
        let client = match reqwest::blocking::ClientBuilder::new()
            .cookie_store(true)
            .gzip(true)
            .build()
        {
            Ok(client) => client,
            Err(error) => panic!("Problem creating HTTP client: {:?}", error),
        };

        Ok(SpeidelClient {
            username,
            password,
            machines: HashMap::new(),
            http_client: client,
        })
    }

    fn add_machine(&mut self, machine_id: u64, api_token: String) {
        self.machines.insert(machine_id, Machine { api_token });
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
            "/auth/login" => panic!("Username or password is incorrect"),
            "/myspeidel/index" => {
                let page = index.text().unwrap();
                let doc = Document::from(page.as_str());

                for machine in doc.find(Class("device-list").descendant(Class("teaser-box-item"))) {
                    let machine_id = machine.attr("data-machine-id").unwrap();

                    let resp = self
                        .http_client
                        .get(format!(
                            "https://www.myspeidel.com/braumeister/control/{}",
                            machine_id
                        ))
                        .send()
                        .unwrap()
                        .text()
                        .unwrap();

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
                                    .unwrap()
                                    .strip_suffix(';')
                                    .unwrap(),
                            )
                            .unwrap()
                        })
                        .collect();

                    for config in bmv2control_data.iter() {
                        let machine_id = config["machineId"]
                            .to_string()
                            .chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<u64>()
                            .unwrap();

                        let api_token = config["apiAuthToken"]
                            .to_string()
                            .chars()
                            .filter(|c| c.is_alphanumeric())
                            .collect::<String>();

                        self.add_machine(machine_id, api_token);
                    }
                }
            }

            _ => panic!("Unable to login for an unknown reason: {:?}", index),
        }

        Ok(())
    }
}

fn main() {
    // Initialize the logger from the environment
    env_logger::init();

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
    for (k, v) in s.machines.iter() {
        machine_id = k.to_string();
        api_token = v.api_token.to_string();
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
                match p["topic"].as_str().unwrap() {
                    "braumeister/machineinfo" => {
                        let m: MachineInfo =
                            serde_json::from_str(p["body"].to_string().as_str()).unwrap();
                        print!("{:?}", m);
                    }
                    "braumeister/status" => {
                        let m: Status =
                            serde_json::from_str(p["body"].to_string().as_str()).unwrap();
                        print!("{:?}", m);
                    }
                    "braumeister/config" => {
                        let m: Config =
                            serde_json::from_str(p["body"].to_string().as_str()).unwrap();
                        print!("{:?}", m);
                    }
                    unknown => {
                        println!("received unknown topic: {}", unknown);
                    }
                }
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
