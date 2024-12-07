use std::net::TcpStream;
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use std::thread;
use std::sync::{Arc, Mutex};
use detours_sys as detours;
use winapi::um::winsock2::*;
use lazy_static::lazy_static;

const XKORE_SERVER_PORT: u16 = 2350;
const BUF_SIZE: usize = 4096;
const TIMEOUT: u64 = 10000;
const RECONNECT_INTERVAL: u64 = 3000;
const PING_INTERVAL: u64 = 5000;
const SLEEP_TIME: u64 = 10;

#[derive(Debug)]
enum PacketType {
    Received,
    Sent,
}

struct NetworkState {
    kore_client: Option<TcpStream>,
    ro_server: Option<TcpStream>,
    kore_alive: bool,
    send_buf: Vec<u8>,
    xkore_send_buf: Vec<u8>,
}

// Global state wrapped in mutex
lazy_static! {
    static ref NETWORK_STATE: Arc<Mutex<NetworkState>> = Arc::new(Mutex::new(NetworkState {
        kore_client: None,
        ro_server: None,
        kore_alive: false,
        send_buf: Vec::new(),
        xkore_send_buf: Vec::new(),
    }));
}

// Original WinAPI functions
static mut ORIGINAL_RECV: Option<unsafe extern "system" fn(SOCKET, *mut i8, i32, i32) -> i32> = None;
static mut ORIGINAL_SEND: Option<unsafe extern "system" fn(SOCKET, *const i8, i32, i32) -> i32> = None;

// Hook implementations
#[no_mangle]
pub unsafe extern "system" fn hooked_recv(socket: SOCKET, buffer: *mut i8, len: i32, flags: i32) -> i32 {
    println!("Called hooked_recv");
    
    let ret_len = if let Some(orig_recv) = ORIGINAL_RECV {
        orig_recv(socket, buffer, len, flags)
    } else {
        return SOCKET_ERROR;
    };

    if ret_len != SOCKET_ERROR && ret_len > 0 {
        let mut state = NETWORK_STATE.lock().unwrap();
        let data = std::slice::from_raw_parts(buffer as *const u8, ret_len as usize);
        send_data_to_kore(&mut state, data, PacketType::Received);
    }

    ret_len
}

#[no_mangle]
pub unsafe extern "system" fn hooked_send(socket: SOCKET, buffer: *const i8, len: i32, flags: i32) -> i32 {
    println!("Called hooked_send");
    
    let ret = if let Some(orig_send) = ORIGINAL_SEND {
        orig_send(socket, buffer, 0, flags)
    } else {
        return SOCKET_ERROR;
    };

    if ret != SOCKET_ERROR && len > 0 {
        let mut state = NETWORK_STATE.lock().unwrap();
        if state.kore_alive {
            let data = std::slice::from_raw_parts(buffer as *const u8, len as usize);
            send_data_to_kore(&mut state, data, PacketType::Sent);
            len
        } else {
            // Send directly to RO server
            if let Some(orig_send) = ORIGINAL_SEND {
                orig_send(socket, buffer, len, flags)
            } else {
                SOCKET_ERROR
            }
        }
    } else {
        ret
    }
}

fn send_data_to_kore(state: &mut NetworkState, buffer: &[u8], packet_type: PacketType) {
    if state.kore_alive {
        let mut new_buf = Vec::with_capacity(buffer.len() + 3);
        match packet_type {
            PacketType::Received => new_buf.push(b'R'),
            PacketType::Sent => new_buf.push(b'S'),
        }
        
        let len = buffer.len() as u16;
        new_buf.extend_from_slice(&len.to_le_bytes());
        new_buf.extend_from_slice(buffer);
        
        state.xkore_send_buf.extend_from_slice(&new_buf);
    }
}

fn kore_connection_main(keep_running: Arc<Mutex<bool>>) {
    let mut buf = [0u8; BUF_SIZE];
    let mut last_ping = Instant::now();
    let mut last_connect_attempt = Instant::now();
    
    while *keep_running.lock().unwrap() {
        let mut state = NETWORK_STATE.lock().unwrap();
        
        // Connection management
        if !state.kore_alive || last_connect_attempt.elapsed() > Duration::from_millis(RECONNECT_INTERVAL) {
            if let Ok(stream) = TcpStream::connect(format!("127.0.0.1:{}", XKORE_SERVER_PORT)) {
                state.kore_client = Some(stream);
                state.kore_alive = true;
                println!("Connected to X-Kore server");
            }
            last_connect_attempt = Instant::now();
        }
        
        // Handle received data
        if let Some(client) = &mut state.kore_client {
            if let Ok(n) = client.read(&mut buf) {
                if n > 0 {
                    process_packet(&buf[..n], &mut state);
                }
            }
        }
        
        // Send pending data
        if !state.xkore_send_buf.is_empty() {
            if let Some(client) = &mut state.kore_client {
                if let Ok(_) = client.write_all(&state.xkore_send_buf) {
                    state.xkore_send_buf.clear();
                }
            }
        }
        
        // Send keep-alive
        if state.kore_alive && last_ping.elapsed() > Duration::from_millis(PING_INTERVAL) {
            if let Some(client) = &mut state.kore_client {
                let ping = [b'K', 0, 0];
                let _ = client.write_all(&ping);
                last_ping = Instant::now();
            }
        }
        
        drop(state);
        thread::sleep(Duration::from_millis(SLEEP_TIME));
    }
}

fn process_packet(data: &[u8], state: &mut NetworkState) {
    if data.len() < 3 {
        return;
    }
    
    match data[0] {
        b'S' => {
            println!("Sending data from OpenKore to Server");
            if let Some(server) = &mut state.ro_server {
                let _ = server.write_all(&data[3..]);
            }
        },
        b'R' => {
            println!("Sending data from OpenKore to Client");
            state.send_buf.extend_from_slice(&data[3..]);
        },
        b'K' => println!("Received Keep-Alive Packet"),
        _ => {}
    }
}

#[no_mangle]
pub extern "system" fn DllMain(_hinst: *mut u8, reason: u32, _: *mut u8) -> i32 {
    match reason {
        1 /* DLL_PROCESS_ATTACH */ => {
            unsafe {
                // Store original function pointers
                ORIGINAL_RECV = Some(recv);
                ORIGINAL_SEND = Some(send);
                
                // Set up hooks
                detours::DetourTransactionBegin();
                detours::DetourUpdateThread(std::mem::transmute(
                    winapi::um::processthreadsapi::GetCurrentThread()
                ));
                
                detours::DetourAttach(&mut (ORIGINAL_RECV.unwrap() as *mut _), 
                                    hooked_recv as *mut _);
                detours::DetourAttach(&mut (ORIGINAL_SEND.unwrap() as *mut _), 
                                    hooked_send as *mut _);
                
                detours::DetourTransactionCommit();
            }
            
            // Start main thread
            let keep_running = Arc::new(Mutex::new(true));
            let keep_running_clone = keep_running.clone();
            
            thread::spawn(move || {
                kore_connection_main(keep_running_clone);
            });
        },
        0 /* DLL_PROCESS_DETACH */ => {
            unsafe {
                // Remove hooks
                detours::DetourTransactionBegin();
                detours::DetourUpdateThread(std::mem::transmute(
                    winapi::um::processthreadsapi::GetCurrentThread()
                ));
                
                if let Some(orig_recv) = ORIGINAL_RECV {
                    detours::DetourDetach(&mut (orig_recv as *mut _), 
                                        hooked_recv as *mut _);
                }
                if let Some(orig_send) = ORIGINAL_SEND {
                    detours::DetourDetach(&mut (orig_send as *mut _), 
                                        hooked_send as *mut _);
                }
                
                detours::DetourTransactionCommit();
            }
        },
        _ => {}
    }
    1
}