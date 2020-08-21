use embedded_hal::{digital::v2::OutputPin, serial::Read, serial::Write};

use crate::protocol::{
    Command, ConnectionType, FirmwareInfo, IpAddresses, Response, WifiConnectionFailure,
};

use heapless::{
    consts::{U16, U2},
    spsc::{Consumer, Queue},
};

use log::info;

use crate::adapter::AdapterError::UnableToInitialize;
use crate::adapter::SocketError::{NoAvailableSockets, ReadError, SocketNotOpen, WriteError};
use crate::ingress::Ingress;
use crate::network::NetworkStack;
use drogue_network::SocketAddr;

#[derive(Debug)]
pub enum AdapterError {
    Timeout,
    UnableToInitialize,
    WriteError,
}

#[derive(Debug)]
pub enum SocketError {
    NoAvailableSockets,
    SocketNotOpen,
    UnableToOpen,
    WriteError,
    ReadError,
}

#[derive(Debug)]
enum SocketState {
    HalfClosed,
    Closed,
    Open,
    Connected,
}

type Initialized<'a, Tx, Rx> = (Adapter<'a, Tx>, Ingress<'a, Rx>);

/// Initialize an ESP8266 board for usage as a Wifi-offload device.
///
/// * tx: Serial transmitter.
/// * rx: Serial receiver.
/// * enable_pin: Pin connected to the ESP's `en` pin.
/// * reset_pin: Pin connect to the ESP's `rst` pin.
/// * response_queue: Queue for inbound AT command responses.
/// * notification_queue: Queue for inbound unsolicited AT notification messages.
pub fn initialize<'a, Tx, Rx, EnablePin, ResetPin>(
    mut tx: Tx,
    mut rx: Rx,
    enable_pin: &mut EnablePin,
    reset_pin: &mut ResetPin,
    response_queue: &'a mut Queue<Response, U2>,
    notification_queue: &'a mut Queue<Response, U16>,
) -> Result<Initialized<'a, Tx, Rx>, AdapterError>
where
    Tx: Write<u8>,
    Rx: Read<u8>,
    EnablePin: OutputPin,
    ResetPin: OutputPin,
{
    let mut buffer: [u8; 1024] = [0; 1024];
    let mut pos = 0;

    const READY: [u8; 7] = *b"ready\r\n";

    let mut counter = 0;

    enable_pin
        .set_high()
        .map_err(|_| AdapterError::UnableToInitialize)?;
    reset_pin
        .set_high()
        .map_err(|_| AdapterError::UnableToInitialize)?;

    loop {
        let result = rx.read();
        match result {
            Ok(c) => {
                buffer[pos] = c;
                pos += 1;
                if pos >= READY.len() && buffer[pos - READY.len()..pos] == READY {
                    log::debug!("adapter is ready");
                    disable_echo(&mut tx, &mut rx)?;
                    enable_mux(&mut tx, &mut rx)?;
                    set_recv_mode(&mut tx, &mut rx)?;
                    return Ok(build_adapter_and_ingress(
                        tx,
                        rx,
                        response_queue,
                        notification_queue,
                    ));
                }
            }
            Err(nb::Error::WouldBlock) => {
                continue;
            }
            Err(_) if counter > 10_000 => {
                break;
            }
            Err(_) => {
                counter += 1;
            }
        }
    }

    Err(AdapterError::UnableToInitialize)
}

fn build_adapter_and_ingress<'a, Tx, Rx>(
    tx: Tx,
    rx: Rx,
    response_queue: &'a mut Queue<Response, U2>,
    notification_queue: &'a mut Queue<Response, U16>,
) -> Initialized<'a, Tx, Rx>
where
    Tx: Write<u8>,
    Rx: Read<u8>,
{
    let (response_producer, response_consumer) = response_queue.split();
    let (notification_producer, notification_consumer) = notification_queue.split();
    (
        Adapter {
            tx,
            response_consumer,
            notification_consumer,
            sockets: initialize_sockets(),
        },
        Ingress::new(rx, response_producer, notification_producer),
    )
}

fn initialize_sockets() -> [Socket; 5] {
    [
        Socket {
            state: SocketState::Closed,
            available: 0,
        },
        Socket {
            state: SocketState::Closed,
            available: 0,
        },
        Socket {
            state: SocketState::Closed,
            available: 0,
        },
        Socket {
            state: SocketState::Closed,
            available: 0,
        },
        Socket {
            state: SocketState::Closed,
            available: 0,
        },
    ]
}

fn write_command<Tx>(tx: &mut Tx, cmd: &[u8]) -> Result<(), Tx::Error>
where
    Tx: Write<u8>,
{
    for b in cmd.iter() {
        nb::block!(tx.write(*b))?;
    }
    Ok(())
}

fn disable_echo<Tx, Rx>(tx: &mut Tx, rx: &mut Rx) -> Result<(), AdapterError>
where
    Tx: Write<u8>,
    Rx: Read<u8>,
{
    write_command(tx, b"ATE0\r\n").map_err(|_| UnableToInitialize)?;
    Ok(wait_for_ok(rx).map_err(|_| UnableToInitialize)?)
}

fn enable_mux<Tx, Rx>(tx: &mut Tx, rx: &mut Rx) -> Result<(), AdapterError>
where
    Tx: Write<u8>,
    Rx: Read<u8>,
{
    write_command(tx, b"AT+CIPMUX=1\r\n").map_err(|_| UnableToInitialize)?;
    Ok(wait_for_ok(rx).map_err(|_| UnableToInitialize)?)
}

fn set_recv_mode<Tx, Rx>(tx: &mut Tx, rx: &mut Rx) -> Result<(), AdapterError>
where
    Tx: Write<u8>,
    Rx: Read<u8>,
{
    write_command(tx, b"AT+CIPRECVMODE=1\r\n").map_err(|_| UnableToInitialize)?;
    Ok(wait_for_ok(rx).map_err(|_| UnableToInitialize)?)
}

fn wait_for_ok<Rx>(rx: &mut Rx) -> Result<(), Rx::Error>
where
    Rx: Read<u8>,
{
    let mut buf: [u8; 64] = [0; 64];
    let mut pos = 0;

    loop {
        let b = nb::block!(rx.read())?;
        buf[pos] = b;
        pos += 1;
        if buf[0..pos].ends_with(b"OK\r\n") {
            log::info!("matched OK");
            return Ok(());
        }
    }
}

struct Socket {
    state: SocketState,
    available: usize,
}

impl Socket {
    pub fn is_closed(&self) -> bool {
        matches!(self.state, SocketState::Closed)
    }

    pub fn is_half_closed(&self) -> bool {
        matches!(self.state, SocketState::HalfClosed)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, SocketState::Open)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, SocketState::Connected)
    }
}

pub struct Adapter<'a, Tx>
where
    Tx: Write<u8>,
{
    tx: Tx,
    response_consumer: Consumer<'a, Response, U2>,
    notification_consumer: Consumer<'a, Response, U16>,
    sockets: [Socket; 5],
}

impl<'a, Tx> Adapter<'a, Tx>
where
    Tx: Write<u8>,
{
    fn send<'c>(&mut self, command: Command<'c>) -> Result<Response, AdapterError> {
        let bytes = command.as_bytes();

        info!(
            "writing command {}",
            core::str::from_utf8(bytes.as_bytes()).unwrap()
        );
        for b in bytes.as_bytes().iter() {
            nb::block!(self.tx.write(*b)).map_err(|_| AdapterError::WriteError)?;
        }
        nb::block!(self.tx.write(b'\r')).map_err(|_| AdapterError::WriteError)?;
        nb::block!(self.tx.write(b'\n')).map_err(|_| AdapterError::WriteError)?;
        self.wait_for_response()
    }

    fn wait_for_response(&mut self) -> Result<Response, AdapterError> {
        loop {
            // busy loop until a response is received.
            let response = self.response_consumer.dequeue();
            match response {
                None => {
                    continue;
                }
                Some(response) => {
                    return Ok(response);
                }
            }
        }
    }

    /// Retrieve the firmware version for the adapter.
    pub fn get_firmware_info(&mut self) -> Result<FirmwareInfo, ()> {
        let command = Command::QueryFirmwareInfo;

        if let Ok(Response::FirmwareInfo((info))) = self.send(command) {
            return Ok(info);
        }

        Err(())
    }

    /// Get the board's IP address. Only valid if connected to an access-point.
    pub fn get_ip_address(&mut self) -> Result<IpAddresses, ()> {
        let command = Command::QueryIpAddress;

        if let Ok(Response::IpAddresses(addresses)) = self.send(command) {
            return Ok(addresses);
        }

        Err(())
    }

    /// Join a wifi access-point.
    ///
    /// The board will expect to obtain an IP address from DHCP.
    ///
    /// * `ssid`: The access-point's SSID to join
    /// * `password`: The password for the access-point.
    pub fn join<'c>(
        &mut self,
        ssid: &'c str,
        password: &'c str,
    ) -> Result<(), WifiConnectionFailure> {
        let command = Command::JoinAp { ssid, password };

        if let Ok(response) = self.send(command) {
            if let Response::Ok = response {
                Ok(())
            } else if let Response::WifiConnectionFailure(reason) = response {
                Err(reason)
            } else {
                Err(WifiConnectionFailure::ConnectionFailed)
            }
        } else {
            Err(WifiConnectionFailure::ConnectionFailed)
        }
    }

    /// Consume the adapter and produce a `NetworkStack`.
    pub fn into_network_stack(self) -> NetworkStack<'a, Tx> {
        NetworkStack::new(self)
    }

    // ----------------------------------------------------------------------
    // TCP Stack
    // ----------------------------------------------------------------------

    fn process_notifications(&mut self) {
        while let Some(response) = self.notification_consumer.dequeue() {
            match response {
                Response::DataAvailable { link_id, len } => {
                    info!("** data avail {} {}", link_id, len);
                    self.sockets[link_id].available += len;
                }
                Response::Connect(_) => {}
                Response::Closed(link_id) => {
                    info!("** close {}", link_id);
                    match self.sockets[link_id].state {
                        SocketState::HalfClosed => {
                            info!("** fully closing");
                            self.sockets[link_id].state = SocketState::Closed;
                        }
                        SocketState::Open | SocketState::Connected => {
                            info!("** half closing");
                            self.sockets[link_id].state = SocketState::HalfClosed;
                        }
                        SocketState::Closed => {
                            info!("** really really closed");
                            // nothing
                        }
                    }
                }
                _ => { /* ignore */ }
            }
        }
    }

    pub(crate) fn open(&mut self) -> Result<usize, SocketError> {
        if let Some((index, socket)) = self
            .sockets
            .iter_mut()
            .enumerate()
            .find(|(_, e)| e.is_closed())
        {
            socket.state = SocketState::Open;
            return Ok(index);
        }

        Err(NoAvailableSockets)
    }

    pub(crate) fn close(&mut self, link_id: usize) -> Result<(), SocketError> {
        self.sockets[link_id].state = SocketState::Closed;
        Ok(())
    }

    pub(crate) fn connect_tcp(
        &mut self,
        link_id: usize,
        remote: SocketAddr,
    ) -> Result<(), SocketError> {
        let command = Command::StartConnection(link_id, ConnectionType::TCP, remote);
        if let Ok(response) = self.send(command) {
            if let Response::Connect(..) = response {
                self.sockets[link_id].state = SocketState::Connected;
                return Ok(());
            }
        }

        Err(SocketError::UnableToOpen)
    }

    pub(crate) fn write(
        &mut self,
        link_id: usize,
        buffer: &[u8],
    ) -> nb::Result<usize, SocketError> {
        self.process_notifications();

        let command = Command::Send {
            link_id,
            len: buffer.len(),
        };

        if let Ok(response) = self.send(command) {
            if let Response::Ok = response {
                if let Ok(response) = self.wait_for_response() {
                    if let Response::ReadyForData = response {
                        info!("sending data {}", buffer.len());
                        for b in buffer.iter() {
                            nb::block!(self.tx.write(*b))
                                .map_err(|_| nb::Error::from(WriteError))?;
                        }
                        info!("sent data {}", buffer.len());
                        if let Ok(response) = self.wait_for_response() {
                            if let Response::SendOk(len) = response {
                                return Ok(len);
                            }
                        }
                    }
                }
            }
        }
        Err(nb::Error::from(SocketError::WriteError))
    }

    pub(crate) fn read(
        &mut self,
        link_id: usize,
        buffer: &mut [u8],
    ) -> nb::Result<usize, SocketError> {
        self.process_notifications();

        if let SocketState::Closed = self.sockets[link_id].state {
            return Err(nb::Error::Other(SocketNotOpen));
        }

        if let SocketState::HalfClosed = self.sockets[link_id].state {
            if self.sockets[link_id].available == 0 {
                return Err(nb::Error::Other(SocketNotOpen));
            }
        }

        if self.sockets[link_id].available == 0 {
            return Err(nb::Error::WouldBlock);
        }

        info!("read {} with {}", link_id, self.sockets[link_id].available);

        let command = Command::Receive {
            link_id,
            len: buffer.len(),
        };

        match self.send(command) {
            Ok(response) => match response {
                Response::DataReceived(inbound, len) => {
                    for (i, b) in inbound[0..len].iter().enumerate() {
                        buffer[i] = *b;
                    }
                    self.sockets[link_id].available -= len;
                    Ok(len)
                }
                _ => Err(nb::Error::Other(ReadError)),
            },
            Err(_) => Err(nb::Error::Other(ReadError)),
        }
    }
}
