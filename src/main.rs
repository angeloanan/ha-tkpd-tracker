#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![warn(clippy::cargo)]
#![warn(clippy::perf)]
#![warn(clippy::complexity)]
#![warn(clippy::style)]

use std::fmt;
use std::io::Write;
use std::time::Duration;

use blake2::Blake2sVar;
use blake2::digest::VariableOutput;
use clap::{Parser, ValueHint, command};
use log::{error, info, trace, warn};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, HOST, HeaderMap, HeaderValue, REFERER};
use rumqttc::MqttOptions;
use serde_json::{Value, json};

/// Tracks Tokopedia item prices via Home Assistant
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The Tokopedia URL for a price to be tracked
    #[arg(value_hint(ValueHint::Url))]
    url: String,

    /// MQTT Broker username if required
    #[arg(long("username"), short('u'), value_hint(ValueHint::Username))]
    mqtt_username: Option<String>,
    /// MQTT Broker password if required
    #[arg(long("password"), short('p'))]
    mqtt_password: Option<String>,

    /// MQTT Broker host or IP
    #[arg(
        long("server"),
        short('s'),
        value_hint(ValueHint::Hostname),
        default_value = "localhost"
    )]
    mqtt_server: String,
    /// MQTT Broker port
    #[arg(long("port"), short('x'), default_value_t = 1883)]
    mqtt_port: u16,

    /// HA MQTT autodiscover topic
    #[arg(long("topic"), short('t'), default_value = "homeassistant")]
    ha_mqtt_discovery_topic: String,
}

const TKPD_GQL_ENDPOINT: &str = "https://gql.tokopedia.com/graphql/PDPGetLayoutQuery";
const GQL_PDP_OPNAME: &str = "PDPGetLayoutQuery";
const GQL_PDP_QUERY: &str = "fragment ProductHighlight on pdpDataProductContent {\n  name\n  price {\n    value\n    currency\n    priceFmt\n    slashPriceFmt\n    discPercentage\n    __typename\n  }\n  campaign {\n    campaignID\n    campaignType\n    campaignTypeName\n    campaignIdentifier\n    background\n    percentageAmount\n    originalPrice\n    discountedPrice\n    originalStock\n    stock\n    stockSoldPercentage\n    threshold\n    startDate\n    endDate\n    endDateUnix\n    appLinks\n    isAppsOnly\n    isActive\n    hideGimmick\n    showStockBar\n    __typename\n  }\n  thematicCampaign {\n    additionalInfo\n    background\n    campaignName\n    icon\n    __typename\n  }\n  stock {\n    useStock\n    value\n    stockWording\n    __typename\n  }\n  variant {\n    isVariant\n    parentID\n    __typename\n  }\n  wholesale {\n    minQty\n    price {\n      value\n      currency\n      __typename\n    }\n    __typename\n  }\n  isCashback {\n    percentage\n    __typename\n  }\n  isTradeIn\n  isOS\n  isPowerMerchant\n  isWishlist\n  isCOD\n  preorder {\n    duration\n    timeUnit\n    isActive\n    preorderInDays\n    __typename\n  }\n  __typename\n}\n\nquery PDPGetLayoutQuery($shopDomain: String, $productKey: String, $layoutID: String, $apiVersion: Float, $userLocation: pdpUserLocation, $extParam: String, $tokonow: pdpTokoNow, $deviceID: String) {\n  pdpGetLayout(shopDomain: $shopDomain, productKey: $productKey, layoutID: $layoutID, apiVersion: $apiVersion, userLocation: $userLocation, extParam: $extParam, tokonow: $tokonow, deviceID: $deviceID) {\n    name\n    components {\n      name\n      type\n      position\n      data {\n        ...ProductHighlight\n        __typename\n      }\n      __typename\n    }\n    __typename\n  }\n}";
const AKAMAI_HEADER: &str = "pdpGetLayout";
const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    if args.mqtt_password.is_some() && args.mqtt_username.is_none() {
        panic!("MQTT Broker password is provided without any username. Aborting...");
    }
    if args.mqtt_username.is_some() && args.mqtt_password.is_none() {
        warn!("MQTT Broker username is provided without password. Continuing...")
    }
    let http_client = Client::builder()
        .use_rustls_tls()
        .user_agent(USER_AGENT_VALUE)
        .danger_accept_invalid_certs(true) // Cringe
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let url = match reqwest::Url::parse(&args.url) {
        Ok(a) => a,
        Err(e) => {
            error!("Unable to parse URL - {e}");
            return;
        }
    };

    if url
        .host_str()
        .is_none_or(|u| u != "tokopedia.com" && u != "www.tokopedia.com")
    {
        println!("{:?}", url.host_str());
        panic!("Wrong URL - This tool currently only supports tokopedia.com urls")
    }
    let Some(mut path_segment) = url.path_segments() else {
        panic!("Wrong URL format - Seems like you've pasted in a base URL")
    };
    let Some(shop_domain) = path_segment.next() else {
        panic!("Wrong URL format - Shop domain is empty. Did you copy the right URL?");
    };
    let Some(product_key) = path_segment.next() else {
        panic!("Wrong URL format - Product key is empty. Did you copy a product URL?")
    };

    info!("Shop: {shop_domain}");
    info!("Product key: {product_key}");

    let mut hasher = Blake2sVar::new(4).unwrap();
    hasher.write_all(shop_domain.as_bytes()).unwrap();
    hasher.write_all(product_key.as_bytes()).unwrap();
    let product_hash = hasher.finalize_boxed();
    info!("Hash: {:x}", HexSlice(&product_hash));

    let tokopedia_query = json!({
        "query": GQL_PDP_QUERY,
        "operationName": GQL_PDP_OPNAME,
        "variables": {
            "shopDomain": shop_domain,
            "productKey": product_key,
            "apiVersion": 1,
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(HOST, HeaderValue::from_static("gql.tokopedia.com"));
    headers.insert(
        REFERER,
        HeaderValue::from_str(&format!(
            "https://www.tokopedia.com/{shop_domain}/{product_key}"
        ))
        .unwrap(),
    );
    headers.insert("x-tkpd-akamai", HeaderValue::from_static(AKAMAI_HEADER));

    info!("Sending Tokopedia API request");
    let response = http_client
        .post(TKPD_GQL_ENDPOINT)
        .headers(headers)
        .body(tokopedia_query.to_string())
        .send()
        .expect("Failed to send request");

    info!("HTTP response received!");
    let body: Value = response.json().expect("Failed to read response text");
    trace!("{}", body);

    // Handle Error
    if let Some(err) = &body.get("errors") {
        let first_error = err.get(0).expect("Ada error tapi gaada error woi");
        let message = first_error
            .get("message")
            .expect("Woi ada error tapi messagenya gaada goblok ini toped");
        panic!("Unable to fetch product data - {message}")
    };

    let component = &body["data"]["pdpGetLayout"]["components"];
    let Some(data) = component
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c.get("name").unwrap() == "product_content")
        .and_then(|c| c.get("data"))
        .and_then(|d| d.get(0))
    else {
        panic!(
            "Unable to fetch product content detail - It seems like Tokopedia changed their API!"
        )
    };

    println!("{}", data);
    let product_name = data["name"]
        .as_str()
        .expect("Unable to decode product name");
    let product_price = data["price"]["value"]
        .as_i64()
        .expect("Unable to decode product price");
    let product_stock = data["stock"]["value"]
        .as_str()
        .and_then(|f| f.parse::<i64>().ok())
        .expect("Unable to decode product stock");

    info!("Product name: {}", product_name);
    info!("Price: Rp. {product_price}");
    info!("Stock: {product_stock}");

    let mut mqtt_opts = MqttOptions::new(
        format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        args.mqtt_server,
        args.mqtt_port,
    );

    if args.mqtt_username.is_some() {
        info!("Using provided MQTT credentials");
        mqtt_opts.set_credentials(
            args.mqtt_username.unwrap(),
            args.mqtt_password.unwrap_or("".to_string()),
        );
    }
    mqtt_opts.set_keep_alive(Duration::from_secs(10));

    let (client, mut connection) = rumqttc::Client::new(mqtt_opts, 2);

    let thread = std::thread::Builder::new()
        .name("MQTTEventLoop".to_string())
        .spawn(move || {
            for (_i, notification) in connection.iter().enumerate() {
                if let Err(e) = notification {
                    panic!("Error {e}");
                }
                println!("Notification = {:?}", notification);
            }
        })
        .expect("Unable to spawn MQTT sender thread");

    // Product name
    client
        .publish(
            format!(
                "{}/sensor/tkpd-{:x}/name/config",
                args.ha_mqtt_discovery_topic,
                HexSlice(&product_hash)
            ),
            rumqttc::QoS::AtLeastOnce,
            true,
            json!({
                "origin": {
                    "name": env!("CARGO_PKG_NAME"),
                    "support_url": env!("CARGO_PKG_HOMEPAGE"),
                    "sw_version": env!("CARGO_PKG_VERSION")
                },
                "device": {
                    "identifiers": format!("tkpdprice-{:x}", HexSlice(&product_hash)),
                    "serial_number": format!("{}/{}", shop_domain, product_key)
                },
                "platform": "sensor",
                "force_update": true,
                "unique_id": format!("tkpdprice-{:x}-name", HexSlice(&product_hash)),
                "command_topic": format!("tkpdprice/{:x}/name", HexSlice(&product_hash)),
                "name": null
            })
            .to_string(),
        )
        .expect("Unable to send monetary config");

    // Product price
    client
        .publish(
            format!(
                "{}/sensor/tkpd-{:x}/price/config",
                args.ha_mqtt_discovery_topic,
                HexSlice(&product_hash)
            ),
            rumqttc::QoS::AtLeastOnce,
            true,
            json!({
                "origin": {
                    "name": env!("CARGO_PKG_NAME"),
                    "support_url": env!("CARGO_PKG_HOMEPAGE"),
                    "sw_version": env!("CARGO_PKG_VERSION")
                },
                "device": {
                    "identifiers": format!("tkpdprice-{:x}", HexSlice(&product_hash)),
                    "serial_number": format!("{}/{}", shop_domain, product_key)
                },
                "platform": "sensor",
                "device_class": "monetary",
                "unit_of_measurement": "IDR",
                "force_update": true,
                "unique_id": format!("tkpdprice-{:x}-price", HexSlice(&product_hash)),
                "state_topic": format!("tkpdprice/{:x}/price", HexSlice(&product_hash)),
                "name": null
            })
            .to_string(),
        )
        .expect("Unable to send monetary config");

    // Product stock
    client
        .publish(
            format!(
                "{}/sensor/tkpd-{:x}/stock/config",
                args.ha_mqtt_discovery_topic,
                HexSlice(&product_hash)
            ),
            rumqttc::QoS::AtLeastOnce,
            true,
            json!({
                "origin": {
                    "name": env!("CARGO_PKG_NAME"),
                    "support_url": env!("CARGO_PKG_HOMEPAGE"),
                    "sw_version": env!("CARGO_PKG_VERSION")
                },
                "device": {
                    "identifiers": format!("tkpdprice-{:x}", HexSlice(&product_hash)),
                    "serial_number": format!("{}/{}", shop_domain, product_key)
                },
                "platform": "sensor",
                "force_update": true,
                "unique_id": format!("tkpdprice-{:x}-stock", HexSlice(&product_hash)),
                "state_topic": format!("tkpdprice/{:x}/stock", HexSlice(&product_hash)),
                "name": null
            })
            .to_string(),
        )
        .expect("Unable to send stock config");

    // Send data
    client
        .publish(
            format!("tkpdprice/{:x}/name", HexSlice(&product_hash)),
            rumqttc::QoS::AtLeastOnce,
            true,
            product_name,
        )
        .expect("Unable to update name value");
    client
        .publish(
            format!("tkpdprice/{:x}/price", HexSlice(&product_hash)),
            rumqttc::QoS::AtLeastOnce,
            true,
            product_price.to_string(),
        )
        .expect("Unable to update price value");
    client
        .publish(
            format!("tkpdprice/{:x}/stock", HexSlice(&product_hash)),
            rumqttc::QoS::AtLeastOnce,
            true,
            product_stock.to_string(),
        )
        .expect("Unable to update price value");

    client.disconnect().expect("Unable to disconnect from MQTT");

    thread
        .join()
        .expect("MQTT Event loop exited abnormally. Messages might not be fully published!");

    info!("Everything looks successful. Exiting...")
}

// https://stackoverflow.com/questions/27650312/show-u8-slice-in-hex-representation
struct HexSlice<'a>(&'a [u8]);

impl fmt::LowerHex for HexSlice<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            write!(f, "0x")?;
        }
        for &byte in self.0 {
            write!(f, "{byte:0>2x}")?;
        }
        Ok(())
    }
}
