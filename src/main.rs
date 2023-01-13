use futures::{executor::block_on, stream::StreamExt};
use paho_mqtt as mqtt;
use std::{env, process, time::Duration, error::Error};
use serde::Deserialize;
use serde_json::Value;
use html_parser::{Dom, Result};

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
    pump_speed: f32,
    sensor_pressure: f32,
    target_temperature: Option<f32>,
    temperature: f32,
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

fn login(username: String, password: String) -> Result<()> {

    let client = reqwest::blocking::ClientBuilder::new()
        .cookie_store(true)
        .gzip(true)
        .build()
        .unwrap();

    let params = [("identity", username),("password", password), ("submit", "".to_string())];
    client.post("https://www.myspeidel.com/auth/login")
        .form(&params)
        .send()
        .unwrap();

    println!("{:?}", &params);

    let index = client.get("https://www.myspeidel.com/myspeidel/index")
	.send()
	.unwrap()
	.text()
	.unwrap();


    let json = Dom::parse(index.as_str())?.to_json_pretty()?;

    println!("{}", json);


    Ok(())
}

fn main() {
    // Initialize the logger from the environment
    env_logger::init();

    let host = env::args()
        .nth(1)
        .unwrap_or_else(|| "wss://api.cloud.myspeidel.com:443".to_string());

    let token = env::var("SPEIDEL_TOKEN").unwrap_or_else(|e| {
	println!("The SPEIDEL_TOKEN environment variable must be provided: {:?}", e);
	process::exit(1);
    });

    let username = env::var("SPEIDEL_USERNAME").unwrap_or_else(|e| {
	println!("The SPEIDEL_USERNAME environment variable must be provided: {:?}", e);
	process::exit(1);
    });

    let password = env::var("SPEIDEL_PASSWORD").unwrap_or_else(|e| {
	println!("The SPEIDEL_PASSWORD environment variable must be provided: {:?}", e);
	process::exit(1);
    });

    login(username, password);
	

    let machine_id = env::var("MACHINE_ID").unwrap_or_else(|e| {
	println!("The MACHINE_ID environment variable must be provided: {:?}", e);
	process::exit(1);
    });

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
	    .user_name(token)
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
		let p: Value = match serde_json::from_str(msg.payload_str().into_owned().as_str()) {
		    Ok(v) => v,
		    Err(e) => panic!("Recieved empty message, token may have expired: {e}"),
		};
		match p["topic"].as_str().unwrap() {
		    "braumeister/machineinfo" => {
			let m: MachineInfo = serde_json::from_str(p["body"].to_string().as_str()).unwrap();
			print!("{:?}", m);
		    },
		    "braumeister/status" => {
			let m: Status = serde_json::from_str(p["body"].to_string().as_str()).unwrap();
			print!("{:?}", m);
		    },
		    "braumeister/config" => {
			let m: Config = serde_json::from_str(p["body"].to_string().as_str()).unwrap();
			print!("{:?}", m);
		    },
		    unknown => {
			println!("received unknown topic: {}", unknown);
			
		    }
		}
            }
            else {
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
