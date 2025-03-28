/// Progressor data types
///
/// See [Tindeq API documentation] for more information
///
/// [Tindeq API documentation]: https://tindeq.com/progressor_api/
use arrayvec::ArrayVec;
use bytemuck_derive::{Pod, Zeroable};
use defmt::{debug, error, info, trace, Format};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use esp_hal::time;

use crate::hx711::Hx711;

/// Size of the channel used to send data points
const DATA_POINT_COMMAND_CHANNEL_SIZE: usize = 80;
/// Channel used to send data points
pub type DataPointChannel = Channel<NoopRawMutex, DataPoint, DATA_POINT_COMMAND_CHANNEL_SIZE>;

/// Maximum size of the data payload in bytes for any data point
pub const MAX_PAYLOAD_SIZE: usize = 12;

/// Progressor BLE Scanning Response
pub const SCAN_RESPONSE_DATA: &[u8] = &[
    18, // Length
    17, // AD_FLAG_LE_LIMITED_DISCOVERABLE | SIMUL_LE_BR_HOST
    0x07, 0x57, 0xad, 0xfe, 0x4f, 0xd3, 0x13, 0xcc, 0x9d, 0xc9, 0x40, 0xa6, 0x1e, 0x01, 0x17, 0x4e,
    0x7e, //UUID
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // Padding
];

/// Status of the weight measurement task
#[derive(Copy, Debug, Clone, PartialEq)]
pub enum MeasurementTaskStatus {
    /// Measurements are enabled
    Enabled,
    /// Measurements are disabled
    Disabled,
    /// Device is in calibration mode
    Calibration(f32),
    /// Taring the scale
    ///
    /// Used in ClimbHarder App
    Tare,
    /// Soft taring the scale (subtract the current weight)
    ///
    /// Used in Tindeq App
    SoftTare,
    /// Restores default calibration values
    DefaultCalibration,
}

/// Device state management
#[derive(Copy, Debug, Clone, PartialEq)]
pub struct DeviceState {
    /// Measurement status
    pub measurement_status: MeasurementTaskStatus,
    /// Tared status
    pub tared: bool,
    /// Start time of the measurement
    pub start_time: u32,
    /// Calibration points
    pub calibration_points: [f32; 2],
}

/// Progressor Commands
pub enum ControlOpCode {
    /// Command used to zero weight when no load is applied
    TareScale = 0x64,
    /// Start continuous measurement. Sample rate is 80Hz
    StartMeasurement = 0x65,
    /// Stop weight measurement. This should be done before sampling the battery voltage
    StopMeasurement = 0x66,
    /// Turn the Progressor off
    Shutdown = 0x6E,
    /// Measures the battery voltage in millivolts
    SampleBattery = 0x6F,
    /// Get the Progressor ID
    GetProgressorId = 0x70,
    /// Get the application version
    GetAppVersion = 0x6B,
    /// Get the calibration values
    GetCalibration = 0x72,
    /// Adds a calibration point
    AddCalibrationPoint = 0x73,
    /// Default calibration
    DefaultCalibration = 0x74,
}

impl ControlOpCode {
    /// Process the control operation
    pub fn process(
        self,
        data: &[u8],
        channel: &'static DataPointChannel,
        device_state: &mut DeviceState,
    ) {
        match self {
            ControlOpCode::TareScale => {
                device_state.measurement_status = MeasurementTaskStatus::Tare;
            }
            ControlOpCode::StartMeasurement => {
                device_state.start_time =
                    (time::Instant::now().duration_since_epoch()).as_micros() as u32;
                if device_state.tared {
                    device_state.measurement_status = MeasurementTaskStatus::Enabled;
                } else {
                    device_state.measurement_status = MeasurementTaskStatus::SoftTare;
                }
            }
            ControlOpCode::StopMeasurement => {
                device_state.measurement_status = MeasurementTaskStatus::Disabled;
            }
            ControlOpCode::GetAppVersion => {
                let response = ResponseCode::AppVersion(env!("DEVICE_VERSION_NUMBER").as_bytes());
                debug!("AppVersion: {:#x}", response);
                let data_point = DataPoint::from(response);
                data_point.send(channel);
            }
            ControlOpCode::GetProgressorId => {
                let device_id = env!("DEVICE_ID");
                let id = match device_id.parse::<u64>() {
                    Ok(id) => id,
                    Err(_) => {
                        error!("Failed to parse DEVICE_ID");
                        0 // Default ID in case of parsing error
                    }
                };
                let response = ResponseCode::ProgressorId(id);
                debug!("ProgressorId: {:?}", response);
                let data_point = DataPoint::from(response);
                data_point.send(channel);
            }
            ControlOpCode::GetCalibration => {
                info!("GetCalibration: {:?}", Hx711::get_calibration());
            }
            ControlOpCode::AddCalibrationPoint => {
                if data.len() < 5 {
                    error!("AddCalibrationPoint: Invalid data length");
                    return;
                }

                let weight = match data[1..5].try_into() {
                    Ok(bytes) => f32::from_be_bytes(bytes),
                    Err(e) => {
                        error!("Failed to parse calibration point data: {:?}", e);
                        return;
                    }
                };

                device_state.measurement_status = MeasurementTaskStatus::Calibration(weight);
                debug!(
                    "Received AddCalibrationPoint command with measurement: {}",
                    weight
                );
            }
            ControlOpCode::DefaultCalibration => {
                device_state.measurement_status = MeasurementTaskStatus::DefaultCalibration;
            }
            // Currently unimplemented operations
            ControlOpCode::Shutdown | ControlOpCode::SampleBattery => {}
        }
    }
}

impl From<u8> for ControlOpCode {
    fn from(op_code: u8) -> Self {
        match op_code {
            0x64 => ControlOpCode::TareScale,
            0x65 => ControlOpCode::StartMeasurement,
            0x66 => ControlOpCode::StopMeasurement,
            0x6E => ControlOpCode::Shutdown,
            0x6F => ControlOpCode::SampleBattery,
            0x70 => ControlOpCode::GetProgressorId,
            0x6B => ControlOpCode::GetAppVersion,
            0x72 => ControlOpCode::GetCalibration,
            0x73 => ControlOpCode::AddCalibrationPoint,
            0x74 => ControlOpCode::DefaultCalibration,
            _ => {
                error!("Invalid OpCode received: {:#x}", op_code);
                ControlOpCode::Shutdown
            }
        }
    }
}

impl Format for ControlOpCode {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            ControlOpCode::TareScale => defmt::write!(fmt, "TareScale"),
            ControlOpCode::StartMeasurement => defmt::write!(fmt, "StartMeasurement"),
            ControlOpCode::StopMeasurement => defmt::write!(fmt, "StopMeasurement"),
            ControlOpCode::GetAppVersion => defmt::write!(fmt, "GetAppVersion"),
            ControlOpCode::Shutdown => defmt::write!(fmt, "Shutdown"),
            ControlOpCode::SampleBattery => defmt::write!(fmt, "SampleBattery"),
            ControlOpCode::GetProgressorId => defmt::write!(fmt, "GetProgressorId"),
            ControlOpCode::GetCalibration => defmt::write!(fmt, "GetCalibration"),
            ControlOpCode::AddCalibrationPoint => {
                defmt::write!(fmt, "AddCalibrationPoint")
            }
            ControlOpCode::DefaultCalibration => defmt::write!(fmt, "DefaultCalibration"),
        }
    }
}

#[derive(Copy, Debug, Clone, Pod, Zeroable)]
#[repr(C, packed)]
/// Data point characteristic is where we receive data from the Progressor
pub struct DataPoint {
    /// Response code
    response_code: u8,
    /// Length of the data
    length: u8,
    /// Data
    value: [u8; MAX_PAYLOAD_SIZE],
}

impl DataPoint {
    /// Send data point to the channel
    pub fn send(&self, channel: &'static DataPointChannel) {
        if channel.try_send(*self).is_err() {
            error!("Failed to send data point: channel full or receiver dropped");
        } else {
            trace!("Sent data point successfully");
        }
    }
}

impl Format for DataPoint {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(
            fmt,
            "Code: {}, Length: {}, Data: {:x}",
            self.response_code,
            self.length,
            &self.value[0..self.length as usize]
        );
    }
}

impl From<ResponseCode> for DataPoint {
    fn from(response_code: ResponseCode) -> Self {
        Self {
            length: response_code.length(),
            value: response_code.value(),
            response_code: response_code.op_code(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(u8)]
/// Data point response code
pub enum ResponseCode {
    /// Response to [OpCode::SampleBattery] command
    SampleBatteryVoltage(u32),
    /// Each measurement is sent together with a timestamp where the timestamp is the number of microseconds since the measurement was started
    WeightMeasurement(f32, u32),
    /// Low power warning indicating that the battery is empty. The Progressor will turn itself off after sending this warning
    LowPowerWarning,
    /// Response to [OpCode::GetAppVersion] command
    AppVersion(&'static [u8]),
    /// Response to [OpCode::GetProgressorId] command
    ProgressorId(u64),
}

impl Format for ResponseCode {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            ResponseCode::SampleBatteryVoltage(voltage) => {
                defmt::write!(fmt, "SampleBatteryVoltage: {}", voltage)
            }
            ResponseCode::WeightMeasurement(weight, timestamp) => {
                defmt::write!(
                    fmt,
                    "WeightMeasurement: Weight: {}, Timestamp: {}",
                    weight,
                    timestamp
                )
            }
            ResponseCode::LowPowerWarning => defmt::write!(fmt, "LowPowerWarning"),
            ResponseCode::AppVersion(version) => defmt::write!(fmt, "AppVersion: {:?}", version),
            ResponseCode::ProgressorId(id) => defmt::write!(fmt, "ProgressorId({})", id),
        }
    }
}

impl ResponseCode {
    fn op_code(&self) -> u8 {
        match self {
            ResponseCode::SampleBatteryVoltage(..)
            | ResponseCode::AppVersion(..)
            | ResponseCode::ProgressorId(..) => 0x00,
            ResponseCode::WeightMeasurement(..) => 0x01,
            ResponseCode::LowPowerWarning => 0x04,
        }
    }

    fn length(&self) -> u8 {
        match self {
            ResponseCode::SampleBatteryVoltage(..) => 4,
            ResponseCode::WeightMeasurement(..) => 8,
            ResponseCode::LowPowerWarning => 0,
            ResponseCode::AppVersion(version) => version.len() as u8,
            ResponseCode::ProgressorId(..) => 1,
        }
    }

    fn value(&self) -> [u8; MAX_PAYLOAD_SIZE] {
        let mut value = [0; MAX_PAYLOAD_SIZE];
        match self {
            ResponseCode::SampleBatteryVoltage(voltage) => {
                value[0..4].copy_from_slice(&voltage.to_le_bytes());
            }
            ResponseCode::WeightMeasurement(weight, timestamp) => {
                value[0..4].copy_from_slice(&weight.to_le_bytes());
                value[4..8].copy_from_slice(&timestamp.to_le_bytes());
            }
            ResponseCode::LowPowerWarning => (),
            ResponseCode::ProgressorId(id) => {
                let bytes = to_le_bytes_without_trailing_zeros(*id);
                value[0..bytes.len()].copy_from_slice(&bytes);
            }
            ResponseCode::AppVersion(version) => {
                value[0..version.len()].copy_from_slice(version);
            }
        };
        value
    }
}

/// Convert an integer into an array of bytes with any zeros on the MSB side trimmed
fn to_le_bytes_without_trailing_zeros<T: Into<u64>>(input: T) -> ArrayVec<u8, 8> {
    let input = input.into();
    if input == 0 {
        return ArrayVec::try_from([0_u8].as_slice()).unwrap();
    }
    let mut out: ArrayVec<u8, 8> = input
        .to_le_bytes()
        .into_iter()
        .rev()
        .skip_while(|&i| i == 0)
        .collect();
    out.reverse();
    out
}
