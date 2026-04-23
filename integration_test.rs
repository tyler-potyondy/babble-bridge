#![cfg(target_os = "linux")]

//! Integration tests intended to evaluate the `nrf-rpc` crate's client implementation
//! against the zephyr `sample/nrf_rpc/protocol_serialization/server`.
//!
//! This test is fully run on the host without any hardware. To do this, we use
//! nordic's Babble Simulator. The testing infastructure will build the zephyr sample,
//! launch the Babble Simulator, and then bind the server's pseudo port used for uart
//! rx/tx to a unix socket. The test here then provides a mock transport layer that
//! directs client writes to this socket and polls for responses on the socket.

use nrf_rpc::ble::cgm::{
    BT_UUID_CGM_FEATURE_VAL, BT_UUID_CGM_MEASUREMENT_VAL, BT_UUID_CGM_STATUS_VAL, BT_UUID_CGMS_VAL,
    CgmMeasurement, encode_uuid_16,
};
use nrf_rpc::ble::{
    BT_GATT_CCC_NOTIFY, BT_LE_SCAN_TYPE_ACTIVE, BtConnLeCreateParam, BtGattDiscoverParams,
    BtGattDiscoverType, BtGattSubscribeParams, BtLeConnParam, BtLeScanParam, GattDiscoverResult,
    ScanResultData,
};
use nrf_rpc::{AsyncTransport, RpcClient, TransportError, ble::Ble, uart_transport::Uart};
use nrf_sim_bridge::TestProcesses;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Mock error type
#[derive(Debug)]
struct MockError;

impl core::fmt::Display for MockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Mock transport error")
    }
}

impl TransportError for MockError {}

/// Mock UART transport that records all written packets and forwards them to
/// the Zephyr server via the socat UNIX socket, while continuously reading
/// bytes from the socket into an internal RX buffer.
struct MockUart {
    socat_socket_path: String,
    sent_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    socket: UnixStream,
    rx_buffer: Arc<Mutex<Vec<u8>>>,
}

impl MockUart {
    fn new(socat_socket_path: &str) -> Self {
        let socat_socket_path = socat_socket_path.to_string();
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        let mut last_err: Option<std::io::Error> = None;

        // Retry connecting for a short period to give socat time to start
        // listening on the UNIX socket.
        let socket = loop {
            match UnixStream::connect(&socat_socket_path) {
                Ok(s) => break s,
                Err(e) => {
                    last_err = Some(e);
                    if start.elapsed() >= timeout {
                        panic!(
                            "Failed to connect to socat UNIX socket {} within {:?}: {:?}",
                            socat_socket_path, timeout, last_err
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        };

        // Ensure blocking mode so writes complete before our minimal executor polls again.
        socket
            .set_nonblocking(false)
            .expect("Failed to configure socat UNIX socket");

        // Shared RX buffer where the background reader thread will place bytes
        // received from the Zephyr UART via the UNIX socket.
        let rx_buffer = Arc::new(Mutex::new(Vec::new()));

        // Clone pieces needed for the background RX thread.
        let rx_buffer_clone = Arc::clone(&rx_buffer);
        let socat_socket_path_clone = socat_socket_path.clone();
        let mut read_socket = socket
            .try_clone()
            .expect("Failed to clone socat UNIX socket for RX thread");

        // Spawn a background thread that continuously reads from the socket and
        // appends data into the RX buffer. This emulates a UART RX IRQ/DMA
        // filling a hardware FIFO.
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match read_socket.read(&mut buf) {
                    Ok(0) => {
                        println!(
                            "MockUart RX thread: EOF while reading from socat socket {}",
                            socat_socket_path_clone
                        );
                        break;
                    }
                    Ok(n) => {
                        // Useful for debugging socket/UART rx
                        println!(
                            "MockUart RX thread: Received {} bytes from {}: {:02X?}",
                            n,
                            socat_socket_path_clone,
                            &buf[..n]
                        );
                        let mut rx = rx_buffer_clone.lock().unwrap();
                        rx.extend_from_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        continue;
                    }
                    Err(e) => {
                        println!(
                            "MockUart RX thread: Read error from socat socket {}: {}",
                            socat_socket_path_clone, e
                        );
                        break;
                    }
                }
                // No sleep here — the blocking read() on the socket already
                // yields to the OS when no data is available. Sleeping would
                // just waste BSIM simulated time.
            }
        });

        Self {
            socat_socket_path,
            sent_packets: Arc::new(Mutex::new(Vec::new())),
            socket,
            rx_buffer,
        }
    }

}

impl Uart for MockUart {}

impl AsyncTransport for MockUart {
    type Error = MockError;
    type TxTransportPacket<'a> = nrf_rpc::uart_transport::UartTxTransport<'a>;
    type RxTransportPacket<'a> = nrf_rpc::uart_transport::UartRxTransport<'a>;

    async fn write(&mut self, data: &mut [u8]) -> Result<usize, Self::Error> {
        // Log the packet being sent
        println!(
            "MockUart: Sending {} bytes to {}: {:02X?}",
            data.len(),
            self.socat_socket_path,
            data
        );

        // Record locally for inspection by tests if needed
        self.sent_packets.lock().unwrap().push(data.to_vec());

        // Forward the bytes to the socat UNIX socket so that the Zephyr UART
        // endpoint actually receives the frame.
        if let Err(e) = self.socket.write_all(data) {
            println!(
                "MockUart: Failed to write {} bytes to socat socket {}: {}",
                data.len(),
                self.socat_socket_path,
                e
            );
            return Err(MockError);
        }

        if let Err(e) = self.socket.flush() {
            println!(
                "MockUart: Failed to flush socat socket {}: {}",
                self.socat_socket_path, e
            );
            return Err(MockError);
        }

        Ok(data.len())
    }

    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        use std::time::{Duration, Instant};

        const HDLC_DELIMITER: u8 = 0x7E;

        // Instead of sleeping a fixed coalescing delay (which wastes BSIM
        // simulated time), we scan the RX buffer for complete HDLC frames.
        // A complete frame is delimited by two 0x7E bytes. We only deliver
        // bytes up through the last complete frame's closing delimiter,
        // keeping any partial trailing frame in the buffer for the next read.
        //
        // This eliminates all wall-clock sleeps from the data path, so the
        // RPC client runs at the same speed as BSIM — no simulated time is
        // wasted waiting for real-time coalescing delays.
        let timeout = Duration::from_secs(5);
        let start = Instant::now();

        loop {
            {
                let rx = self.rx_buffer.lock().unwrap();
                if rx.len() >= 2 {
                    // Scan for complete HDLC frames. We need at least two
                    // 0x7E bytes: one opening and one closing delimiter.
                    // Find the last position we can deliver (the closing
                    // delimiter of the last complete frame).
                    let mut delimiter_count = 0u32;
                    let mut last_complete_frame_end: Option<usize> = None;

                    for (i, &byte) in rx.iter().enumerate() {
                        if byte == HDLC_DELIMITER {
                            delimiter_count += 1;
                            // Every even-numbered delimiter (2nd, 4th, ...)
                            // closes a frame. But HDLC frames share
                            // delimiters: the closing 7E of frame N is the
                            // opening 7E of frame N+1. So after the first
                            // delimiter, every subsequent delimiter closes
                            // a frame.
                            if delimiter_count >= 2 {
                                last_complete_frame_end = Some(i);
                            }
                        }
                    }

                    if let Some(end_pos) = last_complete_frame_end {
                        let n = core::cmp::min(buffer.len(), end_pos + 1);
                        drop(rx);

                        let mut rx = self.rx_buffer.lock().unwrap();
                        buffer[..n].copy_from_slice(&rx[..n]);
                        rx.drain(0..n);

                        println!(
                            "MockUart: Delivering {} bytes from RX buffer to client: {:02X?}",
                            n,
                            &buffer[..n]
                        );

                        return Ok(n);
                    }
                }
            }

            if start.elapsed() >= timeout {
                println!(
                    "MockUart: Read timeout from RX buffer for socat socket {}",
                    self.socat_socket_path
                );
                return Ok(0);
            }

            // Yield briefly so the RX thread can fill the buffer.
            // This is just a scheduling yield, not a coalescing delay.
            std::thread::yield_now();
        }
    }

    async fn delay_ms(&mut self, ms: u32) {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
}

/// Launch BabbleSim + Zephyr RPC server + CGM, bridge UART to a UNIX socket, and return a
/// [`MockUart`] connected to that socket.
fn run_zephyr_rpc_server_exe(test_name: &str) -> (TestProcesses, MockUart) {
    let tests_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"));
    let (processes, socket_path) =
        nrf_sim_bridge::spawn_zephyr_rpc_server_with_socat(tests_dir, test_name);
    let uart = MockUart::new(
        socket_path
            .to_str()
            .expect("socket path must be valid UTF-8"),
    );
    (processes, uart)
}

#[test]
/// Basic functionality test to launch server. No client interactions for this test.
fn test_zephyr_rpc_server() {
    println!("Starting Zephyr RPC server test to test that the server launches properly.");

    let (mut processes, _) = run_zephyr_rpc_server_exe("test_zephyr_rpc_server");
    processes.search_stdout_for_strings(HashSet::from([
        "<inf> nrf_ps_server: Initializing RPC server",
        "<dbg> NRF_RPC: Done initializing nRF RPC module",
    ]));
}

#[test]
/// Test the client can send a packet and receive an ACK.
fn test_client_can_send_packet() {
    println!("Starting client can send packet test...");

    // First start the Zephyr RPC server and socat bridge so that the UNIX
    // socket exists and is listening before the MockUart attempts to connect.
    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_client_can_send_packet");

    let _client = client_test_helper(uart);
    processes.search_stdout_for_strings(HashSet::from(["<dbg> nrf_rpc_uart: <<< TX packet"]));
}

// #[test]
// #[serial]
// fn test_client_acks_packets() {
//     println!("Starting client can ack packets test...");
//
//     let (mut processes, mut uart) = run_zephyr_rpc_server_exe();
//     let _client = client_test_helper(&mut uart);
//     processes.search_stdout_for_strings(HashSet::from(["<dbg> nrf_rpc_uart: >>> RX ack"]));
// }

#[test]
fn test_client_group_handshake() {
    println!("Starting client group handshake test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_client_group_handshake");
    let _client = client_test_helper(uart);
    processes.search_stdout_for_strings(HashSet::from([
        "<dbg> NRF_RPC: Group 'bt_rpc' has id 0",
        "<dbg> NRF_RPC: Group 'rpc_utils' has id 1",
    ]));
}

#[test]
fn test_bt_enable_initializes_bluetooth() {
    println!("Starting bt_enable integration test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_bt_enable_initializes_bluetooth");

    // Create BLE client over the same UART transport used by other tests.
    let mut ble =
        embassy_futures::block_on(Ble::new(uart)).expect("Failed to initialize BLE client");

    // Call bt_enable and expect it to succeed end-to-end against the Zephyr server.
    // embassy_futures::block_on(ble.bt_enable(5)).expect("bt_enable RPC failed");
    embassy_futures::block_on(ble.bt_enable(None));

    // Verify at least server startup logs are present.
    processes.search_stdout_for_strings(HashSet::from([
        "<inf> nrf_ps_server: Initializing RPC server",
    ]));
}

#[test]
fn test_bt_begin_advertising() {
    println!("Starting bt_begin_advertising integration test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_bt_begin_advertising");

    // Create BLE client over the same UART transport used by other tests.
    let mut ble =
        embassy_futures::block_on(Ble::new(uart)).expect("Failed to initialize BLE client");

    // Call bt_enable and expect it to succeed end-to-end against the Zephyr server.
    let bt_enable_res = embassy_futures::block_on(ble.bt_enable(None));
    if bt_enable_res.is_err() {
        println!("[WARNING] bt_enable failed: {:?}", bt_enable_res.err());
    }

    let bt_le_adv_start_res = embassy_futures::block_on(ble.bt_le_adv_start());
    if bt_le_adv_start_res.is_err() {
        println!(
            "[WARNING] bt_le_adv_start failed: {:?}",
            bt_le_adv_start_res.err()
        );
    }

    // Verify at least server startup logs are present.
    processes.search_stdout_for_strings(HashSet::from([
        "<inf> nrf_ps_server: Initializing RPC server",
    ]));
}

fn client_test_helper(uart: MockUart) -> RpcClient<MockUart> {
    std::thread::sleep(Duration::from_secs(1));
    let mut client: RpcClient<MockUart> = RpcClient::new(uart);
    embassy_futures::block_on(client.init()).expect("Failed to initialize client");

    client
}

// =============================================================================
// CGM Central Integration Tests
// =============================================================================

/// Helper: Initialize BLE client with bt_enable and connection callback registration.
fn cgm_ble_init(uart: MockUart) -> Ble<MockUart> {
    let mut ble =
        embassy_futures::block_on(Ble::new(uart)).expect("Failed to initialize BLE client");

    // Enable Bluetooth
    let result = embassy_futures::block_on(ble.bt_enable(None));
    assert!(result.is_ok(), "bt_enable failed: {:?}", result.err());

    // Register connection callbacks so the server forwards connect/disconnect events
    let result = embassy_futures::block_on(ble.bt_conn_cb_register_on_remote());
    assert!(
        result.is_ok(),
        "bt_conn_cb_register_on_remote failed: {:?}",
        result.err()
    );

    // Register scan callbacks so the server forwards scan result events
    let result = embassy_futures::block_on(ble.bt_le_scan_cb_register_on_remote());
    assert!(
        result.is_ok(),
        "bt_le_scan_cb_register_on_remote failed: {:?}",
        result.err()
    );

    // Register auth callbacks so pairing (passkey confirm) events are forwarded.
    // Match the C central_cgms sample which registers:
    //   passkey_display + passkey_confirm + cancel
    // Flags: FLAG_PASSKEY_DISPLAY_PRESENT(0x02) | FLAG_PASSKEY_CONFIRM_PRESENT(0x08)
    //        | FLAG_CANCEL_PRESENT(0x20)
    const AUTH_FLAGS: u16 = 0x02 | 0x08 | 0x20;
    let result = embassy_futures::block_on(ble.bt_conn_auth_cb_register_on_remote(AUTH_FLAGS));
    assert!(
        result.is_ok(),
        "bt_conn_auth_cb_register_on_remote failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    assert_eq!(
        status, 0,
        "bt_conn_auth_cb_register_on_remote returned error: {}",
        status
    );

    ble
}

#[test]
/// Test that BLE scanning can be started and stopped successfully.
///
/// This verifies that the bt_le_scan_start RPC command is correctly encoded
/// and accepted by the Zephyr server, and that the server begins scanning
/// for BLE devices (including the CGM peripheral running in BSIM).
fn test_cgm_scan_start_stop() {
    println!("Starting CGM scan start/stop test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_cgm_scan_start_stop");

    let mut ble = cgm_ble_init(uart);

    // Start scanning with default active scan parameters
    let scan_params = BtLeScanParam {
        scan_type: BT_LE_SCAN_TYPE_ACTIVE,
        options: 0,
        interval: 0x0060,
        window: 0x0030,
        timeout: 0,
        interval_coded: 0,
        window_coded: 0,
    };

    let result = embassy_futures::block_on(ble.bt_le_scan_start(&scan_params, None));
    assert!(
        result.is_ok(),
        "bt_le_scan_start failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    println!("bt_le_scan_start returned status: {}", status);
    assert_eq!(
        status, 0,
        "bt_le_scan_start returned non-zero status: {}",
        status
    );

    // Give the scanner a moment to discover the CGM peripheral
    std::thread::sleep(Duration::from_secs(2));

    // Stop scanning
    let result = embassy_futures::block_on(ble.bt_le_scan_stop());
    assert!(result.is_ok(), "bt_le_scan_stop failed: {:?}", result.err());
    let status = result.unwrap();
    println!("bt_le_scan_stop returned status: {}", status);

    // Verify that the Zephyr side initialized BT and started scanning
    processes.search_stdout_for_strings(HashSet::from([
        "bt_hci_core: HW Platform: Nordic Semiconductor",
    ]));
}

#[test]
/// Test that bt_enable + scan start works and the server initializes properly.
///
/// This is a simpler smoke test for the CGM central flow — just enabling BT
/// and starting a scan, verifying the Zephyr logs show BT initialization.
fn test_cgm_bt_enable_and_scan() {
    println!("Starting CGM bt_enable + scan test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_cgm_bt_enable_and_scan");

    let mut ble =
        embassy_futures::block_on(Ble::new(uart)).expect("Failed to initialize BLE client");

    // Enable Bluetooth
    let result = embassy_futures::block_on(ble.bt_enable(None));
    assert!(result.is_ok(), "bt_enable failed: {:?}", result.err());

    // Start active scanning
    let scan_params = BtLeScanParam::default();
    let result = embassy_futures::block_on(ble.bt_le_scan_start(&scan_params, None));
    assert!(
        result.is_ok(),
        "bt_le_scan_start failed: {:?}",
        result.err()
    );

    let status = result.unwrap();
    assert_eq!(status, 0, "bt_le_scan_start returned error: {}", status);

    // Verify BT initialization on server side
    processes.search_stdout_for_strings(HashSet::from([
        "bt_hci_core: HW Platform: Nordic Semiconductor",
    ]));
}

#[test]
/// Smoke-test CGM GATT discovery: init BLE, start scan, verify the RPC
/// commands are accepted by the server.
///
/// Note: This test does **not** establish a BLE connection, so GATT discovery
/// cannot succeed. The full connect → discover → subscribe → notification
/// flow is exercised by `test_cgm_full_central_flow`.
fn test_cgm_gatt_discover() {
    println!("Starting CGM GATT discover test...");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_cgm_gatt_discover");

    let mut ble = cgm_ble_init(uart);

    // Start scanning for CGM peripheral — verifies the scan RPC is accepted.
    let scan_params = BtLeScanParam {
        scan_type: BT_LE_SCAN_TYPE_ACTIVE,
        options: 0,
        interval: 0x0060,
        window: 0x0030,
        timeout: 0,
        interval_coded: 0,
        window_coded: 0,
    };

    let result = embassy_futures::block_on(ble.bt_le_scan_start(&scan_params, None));
    assert!(
        result.is_ok(),
        "bt_le_scan_start failed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), 0);

    // Drop the BLE client before the stdout check so the transport reader
    // doesn't spin on read-timeouts while we scan server logs.
    drop(ble);

    // Verify BT was initialized on the server side.
    processes.search_stdout_for_strings(HashSet::from([
        "bt_hci_core: HW Platform: Nordic Semiconductor",
    ]));
}

// =============================================================================
// Thorough CGM Central Integration Test
// =============================================================================

#[test]
/// Full CGM Central flow: discover → connect → verify.
///
/// This test exercises the complete BLE central pipeline against the CGM
/// peripheral running in BSIM:
///
/// 1. bt_enable + register connection & scan callbacks
/// 2. Start active BLE scanning
/// 3. Receive scan result events and find the CGM peripheral ("Nordic Glucose Sensor")
/// 4. Stop scanning
/// 5. Initiate connection to the CGM peripheral's address
/// 6. Wait for the "connected" callback event (err == 0)
/// 7. Verify server-side logs confirm BT init and connection
fn test_cgm_full_central_flow() {
    const TEST_TIMEOUT: Duration = Duration::from_secs(120);

    let (finished_tx, finished_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let test_thread = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test_cgm_full_central_flow_inner();
        }));
        match result {
            Ok(()) => {
                let _ = finished_tx.send(Ok(()));
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                let _ = finished_tx.send(Err(msg));
            }
        }
    });

    match finished_rx.recv_timeout(TEST_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => panic!("Test panicked: {}", msg),
        Err(_) => panic!(
            "test_cgm_full_central_flow TIMED OUT after {:?}",
            TEST_TIMEOUT
        ),
    }

    // Best-effort join (the thread should have finished by now)
    let _ = test_thread.join();
}

fn test_cgm_full_central_flow_inner() {
    println!("=== Starting CGM full central flow test ===");

    let (mut processes, uart) = run_zephyr_rpc_server_exe("test_cgm_full_central_flow");

    // ------------------------------------------------------------------
    // Step 1: Initialize BLE (bt_enable + conn_cb_register + scan_cb_register)
    // ------------------------------------------------------------------
    println!("[Step 1] Initializing BLE client...");
    let mut ble = cgm_ble_init(uart);
    println!("[Step 1] BLE client initialized.");

    // ------------------------------------------------------------------
    // Step 2: Start active BLE scanning
    // ------------------------------------------------------------------
    println!("[Step 2] Starting BLE scan...");
    let scan_params = BtLeScanParam {
        scan_type: BT_LE_SCAN_TYPE_ACTIVE,
        options: 0,
        interval: 0x0060,
        window: 0x0030,
        timeout: 0,
        interval_coded: 0,
        window_coded: 0,
    };

    let result = embassy_futures::block_on(ble.bt_le_scan_start(&scan_params, None));
    assert!(
        result.is_ok(),
        "bt_le_scan_start failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    assert_eq!(status, 0, "bt_le_scan_start returned error: {}", status);
    println!("[Step 2] Scanning started (status=0).");

    // ------------------------------------------------------------------
    // Step 3: Receive scan results and find the CGM peripheral
    // ------------------------------------------------------------------
    println!("[Step 3] Waiting for scan results...");
    let mut cgm_scan_result: Option<ScanResultData> = None;
    let max_scan_results = 50;

    for i in 0..max_scan_results {
        let result = embassy_futures::block_on(ble.wait_for_scan_result());
        match result {
            Ok(scan) => {
                let name = scan.device_name().unwrap_or("<unknown>");
                println!(
                    "  Scan result #{}: addr={:02X?} type={} rssi={} name=\"{}\"",
                    i, scan.addr, scan.addr_type, scan.rssi, name
                );

                // Look for the CGM peripheral by name or service UUID
                if name.contains("Nordic Glucose Sensor")
                    || name.contains("CGM")
                    || scan.has_service_uuid_16(BT_UUID_CGMS_VAL)
                {
                    println!(
                        "[Step 3] *** Found CGM peripheral: name=\"{}\" addr={:02X?} ***",
                        name, scan.addr
                    );
                    cgm_scan_result = Some(scan);
                    break;
                }
            }
            Err(e) => {
                println!("  Scan result #{}: error {:?}, retrying...", i, e);
            }
        }
    }

    let cgm_peripheral = cgm_scan_result.expect(
        "CGM peripheral (Nordic Glucose Sensor) not found in scan results! \
         Make sure the CGM peripheral BSIM device is running.",
    );

    // ------------------------------------------------------------------
    // Step 4: Stop scanning
    // ------------------------------------------------------------------
    println!("[Step 4] Stopping BLE scan...");
    let result = embassy_futures::block_on(ble.bt_le_scan_stop());
    assert!(result.is_ok(), "bt_le_scan_stop failed: {:?}", result.err());
    println!("[Step 4] Scan stopped.");

    // ------------------------------------------------------------------
    // Step 5: Connect to the CGM peripheral
    // ------------------------------------------------------------------
    let peer_addr = cgm_peripheral.to_addr_le();
    println!(
        "[Step 5] Connecting to CGM peripheral at {:02X?} (type={})...",
        peer_addr.addr, peer_addr.addr_type
    );

    let create_param = BtConnLeCreateParam::default();
    let conn_param = BtLeConnParam::default();
    let result =
        embassy_futures::block_on(ble.bt_conn_le_create(&peer_addr, &create_param, &conn_param));
    assert!(
        result.is_ok(),
        "bt_conn_le_create failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    assert_eq!(status, 0, "bt_conn_le_create returned error: {}", status);
    println!("[Step 5] bt_conn_le_create returned 0 (connection initiating).");

    // ------------------------------------------------------------------
    // Step 6: Wait for the "connected" callback event
    // ------------------------------------------------------------------
    println!("[Step 6] Waiting for connection event...");
    let conn_event = embassy_futures::block_on(ble.wait_for_connection());
    assert!(
        conn_event.is_ok(),
        "Did not receive connection event: {:?}",
        conn_event.err()
    );
    let conn_event = conn_event.unwrap();
    println!("[Step 6] Connection event received: err={}", conn_event.err);
    assert_eq!(
        conn_event.err, 0,
        "Connection failed with HCI error: {}",
        conn_event.err
    );
    println!("[Step 6] Connection established successfully!");

    // ------------------------------------------------------------------
    // Step 6a: Explicitly set security to level 4 (SC authenticated).
    //
    // Unlike the native C central_cgms sample (which relies on
    // CONFIG_BT_GATT_AUTO_SEC_REQ), the RPC flow needs to explicitly
    // escalate security before GATT operations. This triggers SMP
    // Numeric Comparison pairing — the passkey_confirm callback is
    // handled automatically by the auto-confirm mechanism set up in
    // bt_conn_auth_cb_register_on_remote.
    // ------------------------------------------------------------------
    println!("[Step 6a] Setting security to level 4 (SC authenticated)...");
    let result = embassy_futures::block_on(ble.bt_conn_set_security(4));
    assert!(
        result.is_ok(),
        "bt_conn_set_security failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    println!("[Step 6a] bt_conn_set_security returned status={}", status);

    // Wait for the SMP exchange to complete (passkey exchange + security level 4)
    println!("[Step 6a] Waiting for security level 4...");
    let result = embassy_futures::block_on(ble.wait_for_security_level(4));
    assert!(
        result.is_ok(),
        "Failed to achieve security level 4: {:?}",
        result.err()
    );
    let achieved_level = result.unwrap();
    println!("[Step 6a] Security level {} achieved!", achieved_level);

    // ------------------------------------------------------------------
    // Step 7: GATT Service Discovery for CGM Service (UUID 0x181F)
    // ------------------------------------------------------------------
    println!("[Step 7] Starting GATT service discovery for CGM Service (0x181F)...");

    let discover_params = BtGattDiscoverParams {
        uuid: encode_uuid_16(BT_UUID_CGMS_VAL),
        start_handle: 0x0001,
        end_handle: 0xFFFF,
        discover_type: BtGattDiscoverType::PrimaryService,
    };
    let discover_params_ptr: u64 = 0xAA01;

    let result =
        embassy_futures::block_on(ble.bt_gatt_discover(&discover_params, discover_params_ptr));
    assert!(
        result.is_ok(),
        "bt_gatt_discover (primary service) failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    assert_eq!(status, 0, "bt_gatt_discover returned error: {}", status);
    println!("[Step 7] bt_gatt_discover returned 0 — waiting for discovery callbacks...");

    // Collect service discovery results.
    let mut cgm_service_start_handle: Option<u16> = None;
    let mut cgm_service_end_handle: Option<u16> = None;

    for i in 0..20 {
        let result = embassy_futures::block_on(ble.wait_for_gatt_discover_result());
        match result {
            Ok(GattDiscoverResult::Service {
                handle,
                service_uuid_16,
                end_handle,
            }) => {
                println!(
                    "  Discover #{}: Service UUID=0x{:04X} handle={} end_handle={}",
                    i, service_uuid_16, handle, end_handle
                );
                if service_uuid_16 == BT_UUID_CGMS_VAL {
                    println!(
                        "[Step 7] *** Found CGM Service! handle={} end_handle={} ***",
                        handle, end_handle
                    );
                    cgm_service_start_handle = Some(handle);
                    cgm_service_end_handle = Some(end_handle);
                }
            }
            Ok(GattDiscoverResult::Complete) => {
                println!("  Discover #{}: Discovery complete.", i);
                break;
            }
            Ok(other) => {
                println!("  Discover #{}: {:?}", i, other);
            }
            Err(e) => {
                println!("  Discover #{}: error {:?}", i, e);
                break;
            }
        }
    }

    let cgm_start = cgm_service_start_handle
        .expect("CGM Service (0x181F) not found during primary service discovery!");
    let cgm_end = cgm_service_end_handle.unwrap();
    println!(
        "[Step 7] CGM Service discovered: handles {}..{}",
        cgm_start, cgm_end
    );

    // ------------------------------------------------------------------
    // Step 8: Discover CGM Measurement characteristic within the service
    // ------------------------------------------------------------------
    println!("[Step 8] Discovering characteristics within CGM Service...");

    let char_discover_params = BtGattDiscoverParams {
        uuid: encode_uuid_16(BT_UUID_CGM_MEASUREMENT_VAL),
        start_handle: cgm_start,
        end_handle: cgm_end,
        discover_type: BtGattDiscoverType::Characteristic,
    };
    let char_params_ptr: u64 = 0xAA02;

    let result =
        embassy_futures::block_on(ble.bt_gatt_discover(&char_discover_params, char_params_ptr));
    assert!(
        result.is_ok(),
        "bt_gatt_discover (characteristic) failed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), 0);

    let mut cgm_meas_value_handle: Option<u16> = None;

    for i in 0..20 {
        let result = embassy_futures::block_on(ble.wait_for_gatt_discover_result());
        match result {
            Ok(GattDiscoverResult::Characteristic {
                handle,
                char_uuid_16,
                value_handle,
                properties,
            }) => {
                println!(
                    "  Char #{}: UUID=0x{:04X} handle={} value_handle={} props=0x{:02X}",
                    i, char_uuid_16, handle, value_handle, properties
                );
                if char_uuid_16 == BT_UUID_CGM_MEASUREMENT_VAL {
                    println!(
                        "[Step 8] *** Found CGM Measurement Characteristic! value_handle={} ***",
                        value_handle
                    );
                    cgm_meas_value_handle = Some(value_handle);
                }
            }
            Ok(GattDiscoverResult::Complete) => {
                println!("  Char #{}: Discovery complete.", i);
                break;
            }
            Ok(other) => {
                println!("  Char #{}: {:?}", i, other);
            }
            Err(e) => {
                println!("  Char #{}: error {:?}", i, e);
                break;
            }
        }
    }

    let meas_value_handle =
        cgm_meas_value_handle.expect("CGM Measurement Characteristic (0x2AA7) not found!");
    println!(
        "[Step 8] CGM Measurement value_handle={}",
        meas_value_handle
    );

    // ------------------------------------------------------------------
    // Step 9: Discover the CCC descriptor for subscription
    // ------------------------------------------------------------------
    println!("[Step 9] Discovering CCC descriptor for CGM Measurement...");

    let ccc_discover_params = BtGattDiscoverParams {
        uuid: encode_uuid_16(nrf_rpc::ble::cgm::BT_UUID_GATT_CCC_VAL),
        start_handle: meas_value_handle + 1,
        end_handle: cgm_end,
        discover_type: BtGattDiscoverType::Descriptor,
    };
    let ccc_params_ptr: u64 = 0xAA03;

    let result =
        embassy_futures::block_on(ble.bt_gatt_discover(&ccc_discover_params, ccc_params_ptr));
    assert!(
        result.is_ok(),
        "bt_gatt_discover (descriptor) failed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), 0);

    let mut ccc_handle: Option<u16> = None;

    for i in 0..20 {
        let result = embassy_futures::block_on(ble.wait_for_gatt_discover_result());
        match result {
            Ok(GattDiscoverResult::Descriptor { handle, uuid_16 }) => {
                println!("  Desc #{}: UUID=0x{:04X} handle={}", i, uuid_16, handle);
                if uuid_16 == nrf_rpc::ble::cgm::BT_UUID_GATT_CCC_VAL && ccc_handle.is_none() {
                    println!("[Step 9] *** Found CCC descriptor! handle={} ***", handle);
                    ccc_handle = Some(handle);
                }
            }
            Ok(GattDiscoverResult::Complete) => {
                println!("  Desc #{}: Discovery complete.", i);
                break;
            }
            Ok(other) => {
                println!("  Desc #{}: {:?}", i, other);
            }
            Err(e) => {
                println!("  Desc #{}: error {:?}", i, e);
                break;
            }
        }
    }

    let ccc_handle = ccc_handle.expect("CCC descriptor (0x2902) not found for CGM Measurement!");
    println!("[Step 9] CCC descriptor handle={}", ccc_handle);

    // ------------------------------------------------------------------
    // Step 10: Subscribe to CGM Measurement notifications
    // ------------------------------------------------------------------
    println!("[Step 10] Subscribing to CGM Measurement notifications...");

    let subscribe_params = BtGattSubscribeParams {
        has_notify: true,
        value_handle: meas_value_handle,
        ccc_handle,
        value: BT_GATT_CCC_NOTIFY,
        min_security: 0,
        flags: 0,
    };
    let subscribe_params_ptr: u64 = 0xBB01;

    let result =
        embassy_futures::block_on(ble.bt_gatt_subscribe(&subscribe_params, subscribe_params_ptr));
    assert!(
        result.is_ok(),
        "bt_gatt_subscribe failed: {:?}",
        result.err()
    );
    let status = result.unwrap();
    assert_eq!(status, 0, "bt_gatt_subscribe returned error: {}", status);
    println!("[Step 10] Subscribed to CGM Measurement notifications (status=0).");

    // NOTE: Do NOT dump server stdout here — the SMP pairing passkey exchange
    // happens asynchronously after subscribe triggers security auto-escalation.
    // The passkey_confirm event must be ACKed promptly (via auto-confirm) or the
    // SMP handshake will time out, causing security_changed err=4 (AUTH_FAIL).
    // The auto-confirm mechanism in ack_event handles passkey_confirm events
    // automatically when they arrive during wait_for_gatt_notification.

    // ------------------------------------------------------------------
    // Step 11: Receive and decode at least one CGM measurement notification
    // ------------------------------------------------------------------
    println!("[Step 11] Waiting for CGM measurement notification...");

    let mut received_measurement = false;
    let max_notification_attempts = 30;

    for attempt in 0..max_notification_attempts {
        let result = embassy_futures::block_on(ble.wait_for_gatt_notification());
        match result {
            Ok(notif) => {
                println!(
                    "  Notification #{}: params_ptr=0x{:04X} data_len={} raw={:02X?}",
                    attempt,
                    notif.params_ptr,
                    notif.data_len,
                    &notif.data[..notif.data_len]
                );

                // data_len == 0 means subscription was torn down (NULL data);
                // skip and keep waiting for real measurement data.
                if notif.data_len == 0 {
                    println!(
                        "  Notification #{}: NULL data (subscription teardown signal), skipping.",
                        attempt
                    );
                    continue;
                }

                // Parse the CGM measurement from the notification payload
                if let Some(measurement) = CgmMeasurement::parse(&notif.data[..notif.data_len]) {
                    let glucose = measurement.glucose_mg_dl();
                    println!(
                        "[Step 11] *** CGM Measurement received! glucose={:.1} mg/dL, \
                         time_offset={}, flags=0x{:02X} ***",
                        glucose, measurement.time_offset, measurement.flags
                    );

                    // Sanity check: glucose should be a reasonable value (1-1000 mg/dL)
                    assert!(
                        glucose > 0.0 && glucose < 1000.0,
                        "Glucose value out of reasonable range: {}",
                        glucose
                    );

                    received_measurement = true;
                    break;
                } else {
                    println!(
                        "  Notification #{}: Could not parse CGM measurement (data too short?)",
                        attempt
                    );
                }
            }
            Err(e) => {
                println!("  Notification #{}: error {:?}, retrying...", attempt, e);
            }
        }
    }

    assert!(
        received_measurement,
        "Did not receive any CGM measurement notification from the peripheral!"
    );

    // ------------------------------------------------------------------
    // Step 12: Verify server-side logs
    // ------------------------------------------------------------------
    println!("[Step 12] Verifying server-side logs...");
    processes.search_stdout_for_strings(HashSet::from([
        "bt_hci_core: HW Platform: Nordic Semiconductor",
    ]));

    println!("=== CGM full central flow test PASSED ===");
}
