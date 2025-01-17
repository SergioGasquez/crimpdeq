#![no_std]
#![no_main]

use core::cell::RefCell;
use critical_section::Mutex;

use bleps::{
    async_attribute_server::AttributeServer, asynch::Ble, attribute_server::NotificationData, gatt,
};
use bytemuck::bytes_of;
use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex,
    channel::{Channel, Receiver, Sender},
};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    rng::Rng,
    time,
    timer::{systimer::SystemTimer, timg::TimerGroup},
    Config,
};
use esp_println::println;
use esp_wifi::{ble::controller::BleConnector, init, EspWifiController};

use tindeq::progressor::{ControlOpCode, DataPoint, ResponseCode};

// // When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[macro_export]
macro_rules! make_static {
    ($t:ty, $val:expr) => ($crate::make_static!($t, $val,));
    ($t:ty, $val:expr, $(#[$m:meta])*) => {{
        $(#[$m])*
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init_with(|| $val)
    }};
}

const SCAN_RESPONSE_DATA: &[u8] = &[
    18, // Length
    17, 0x07, 0x57, 0xad, 0xfe, 0x4f, 0xd3, 0x13, 0xcc, 0x9d, 0xc9, 0x40, 0xa6, 0x1e, 0x01, 0x17,
    0x4e, 0x7e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const ADVERTISING_DATA: &[u8] = &[
    20, // Length
    2,
    0x01,
    0x02 | 0x04, // LE General Discoverable Mode, BR/EDR Not Supported
    ("Progressor_2639".len() + 1) as u8,
    0x9,
    b'P',
    b'r',
    b'o',
    b'g',
    b'r',
    b'e',
    b's',
    b's',
    b'o',
    b'r',
    b'_',
    b'2',
    b'6',
    b'3',
    b'9',
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
];

pub const MEASURE_COMMAND_CHANNEL_SIZE: usize = 50;
pub type DataPointChannel = Channel<NoopRawMutex, DataPoint, MEASURE_COMMAND_CHANNEL_SIZE>;

// HAVE A STATIC BOOL THAT ENABLES/DISABLES A TASK THAT READS THE WEIGTH
static WEIGTH_TASK_ENABLED: Mutex<RefCell<bool>> = Mutex::new(RefCell::new(false));

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) -> ! {
    let config = Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_println::logger::init_logger_from_env();

    esp_alloc::heap_allocator!(72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let init = &*mk_static!(
        EspWifiController<'static>,
        init(
            timg0.timer0,
            Rng::new(peripherals.RNG),
            peripherals.RADIO_CLK,
        )
        .unwrap()
    );

    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    esp_hal_embassy::init(systimer.alarm0);

    let connector = BleConnector::new(init, peripherals.BT);

    let channel: &DataPointChannel = make_static!(DataPointChannel, Channel::new());

    spawner.spawn(bt_task(connector, channel)).unwrap();
    spawner.spawn(measurement_task(channel)).unwrap();

    loop {
        Timer::after(Duration::from_millis(10)).await;
    }
}

#[embassy_executor::task]
async fn bt_task(connector: BleConnector<'static>, channel: &'static DataPointChannel) {
    let now = || time::now().duration_since_epoch().to_millis();
    let mut ble = Ble::new(connector, now);
    loop {
        println!("ble.init: {:?}", ble.init().await);
        println!(
            "ble.cmd_set_le_advertising_parameters: {:?}",
            ble.cmd_set_le_advertising_parameters().await
        );
        // Todo: See diferences between this and the one below
        // println!(
        //     " ble.cmd_set_le_advertising_data: {:?}",
        //     ble.cmd_set_le_advertising_data(
        //         create_advertising_data(&[
        //             AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
        //             AdStructure::CompleteLocalName("Progressor_2639"),
        //         ])
        //         .unwrap()
        //     )
        // );
        println!(
            "ble.cmd_set_le_advertising_data: {:?}",
            ble.cmd_set_le_advertising_data(Data::new(ADVERTISING_DATA))
                .await
        );

        println!(
            "ble.cmd_set_le_scan_rsp_data: {:?}",
            ble.cmd_set_le_scan_rsp_data(Data::new(SCAN_RESPONSE_DATA))
                .await
        );
        println!(
            "ble.cmd_set_le_advertise_enable: {:?}",
            ble.cmd_set_le_advertise_enable(true).await
        );

        println!("Started advertising");

        let mut data_point_read = |_offset: usize, data: &mut [u8]| {
            data[..20].copy_from_slice(&b"Data Point Read"[..]);
            17
        };

        let mut control_point_write = |_, data: &[u8]| {
            println!("Control Point Received: 0x{:x?}", data);

            match ControlOpCode::from(data[0]) {
                ControlOpCode::TareScale => {
                    println!("TareScale");
                }
                ControlOpCode::StartMeasurement => {
                    println!("StartMeasurement");

                    critical_section::with(|cs| {
                        *WEIGTH_TASK_ENABLED.borrow_ref_mut(cs) = true;
                    });
                }
                ControlOpCode::StopMeasurement => {
                    critical_section::with(|cs| {
                        *WEIGTH_TASK_ENABLED.borrow_ref_mut(cs) = false;
                    });
                    println!("StopMeasurement");
                }
                ControlOpCode::GetAppVersion => {
                    println!("GetAppVersion");
                    // Notify the data_point with the app version
                    let response =
                        ResponseCode::AppVersion(env!("DEVICE_VERSION_NUMBER").as_bytes());
                    let data_point = DataPoint::new(response);
                    if channel.try_send(data_point).is_err() {
                        println!("Failed to send data point");
                    }
                    println!("Sent GetAppVersion");
                }
                ControlOpCode::Shutdown => {
                    println!("Shutdown");
                }
                ControlOpCode::SampleBattery => {
                    println!("SampleBattery");
                }
                ControlOpCode::GetProgressorId => {
                    println!("GetProgressorId");
                    // Notify the data_point with the progressor id
                    let response = ResponseCode::ProgressorId(env!("DEVICE_ID").parse().unwrap());
                    let data_point = DataPoint::new(response);
                    if channel.try_send(data_point).is_err() {
                        println!("Failed to send data point");
                    }
                    println!("Sent GetAppVersion");
                }
            }
        };

        let mut service_change_read = |_offset: usize, data: &mut [u8]| {
            data[..20].copy_from_slice(&b"Service Change Read"[..]);
            17
        };

        // TODO: Avoid using the gatt! macro, replace the uuids with constants and improve the values
        let device_name = b"Progressor_2639";
        let appearance = b"[0] Unknown";
        let ppcp_val = b"Connection Interval: 50.00ms - 65.00ms, Max Latency:6ms, Suppervision Timeout Multiplier: 400ms";
        let car_val = b"Address resolution supported";
        let ccc_vla = b"Notifications and indications disabled";
        gatt!([
            service {
                // Generic Access
                uuid: "1800",
                characteristics: [
                    // Device Name
                    characteristic {
                        uuid: "2a00",
                        value: device_name,
                    },
                    // Appearance
                    characteristic {
                        uuid: "2a01",
                        value: appearance,
                    },
                    // Peripheral Preferred Connection Parameters
                    characteristic {
                        uuid: "2a04",
                        value: ppcp_val,
                    },
                    // Central Address Resolution
                    characteristic {
                        uuid: "2aa6",
                        value: car_val,
                    },
                ],
            },
            // Generic Attribute
            service {
                uuid: "1801",
                characteristics: [
                    // Service Changed
                    characteristic {
                        uuid: "2a05",
                        read: service_change_read,
                        descriptors: [
                            // Client Characteristic Configuration
                            descriptor {
                                uuid: "2902",
                                value: ccc_vla,
                            },
                        ],
                    },
                ],
            },
            service {
                // Progressor Primary
                uuid: "7e4e1701-1ea6-40c9-9dcc-13d34ffead57",
                characteristics: [
                    // Progressor Data Point
                    characteristic {
                        name: "data_point",
                        uuid: "7e4e1702-1ea6-40c9-9dcc-13d34ffead57",
                        notify: true,
                        read: data_point_read,
                    },
                    /// Progressor Control Point
                    characteristic {
                        name: "control_point",
                        uuid: "7e4e1703-1ea6-40c9-9dcc-13d34ffead57",
                        write: control_point_write,
                        // TODO: Is ther a WriteNoResponse?
                    },
                ],
            },
        ]);

        let mut rng = bleps::no_rng::NoRng;
        let mut server = AttributeServer::new(&mut ble, &mut gatt_attributes, &mut rng);

        let mut notifier = || async {
            let data_point = channel.receive().await;
            let data = bytes_of(&data_point);
            NotificationData::new(data_point_handle, data)
        };
        server.run(&mut notifier).await.unwrap();
    }
}

#[embassy_executor::task]
async fn measurement_task(channel: &'static DataPointChannel) {
    let mut counter = 0;
    loop {
        let enabled = critical_section::with(|cs| *WEIGTH_TASK_ENABLED.borrow_ref(cs));

        if enabled {
            println!("Measuring weigth");
            // TODO Measure the weigth
            // Fake data
            counter += 1;
            let weigth = counter as f32 / 10.0;
            let timestamp = (time::now().duration_since_epoch()).to_micros() as u32;
            let measurement = ResponseCode::WeigthtMeasurement(weigth, timestamp);
            let data_point = DataPoint::new(measurement);
            channel.send(data_point).await;
        }
        // Freq is 80Hz so ~13ms
        Timer::after(Duration::from_millis(13)).await;
    }
}
