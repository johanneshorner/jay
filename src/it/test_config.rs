use {
    crate::it::test_error::TestError,
    isnt::std_1::primitive::IsntConstPtrExt,
    jay_config::_private::{
        bincode_ops,
        ipc::{ClientMessage, ServerMessage},
        ConfigEntry, VERSION,
    },
    std::{cell::Cell, ops::Deref, ptr, rc::Rc},
};

pub static TEST_CONFIG_ENTRY: ConfigEntry = ConfigEntry {
    version: VERSION,
    init,
    unref,
    handle_msg,
};

#[thread_local]
static mut CONFIG: *const TestConfig = ptr::null();

pub fn with_test_config<T, F>(f: F) -> T
where
    F: FnOnce(Rc<TestConfig>) -> T,
{
    unsafe {
        let tc = Rc::new(TestConfig {
            srv: Cell::new(None),
        });
        let old = CONFIG;
        CONFIG = tc.deref();
        let res = f(tc.clone());
        CONFIG = old;
        res
    }
}

unsafe extern "C" fn init(
    srv_data: *const u8,
    srv_unref: unsafe extern "C" fn(data: *const u8),
    srv_handler: unsafe extern "C" fn(data: *const u8, msg: *const u8, size: usize),
    _msg: *const u8,
    _size: usize,
) -> *const u8 {
    let tc = CONFIG;
    assert!(tc.is_not_null());
    Rc::increment_strong_count(tc);
    {
        let tc = &*tc;
        tc.srv.set(Some(ServerData {
            srv_data,
            srv_unref,
            srv_handler,
        }));
    }
    tc.cast()
}

unsafe extern "C" fn unref(data: *const u8) {
    Rc::decrement_strong_count(data.cast::<TestConfig>());
}

unsafe extern "C" fn handle_msg(data: *const u8, msg: *const u8, size: usize) {
    let _tc = &*data.cast::<TestConfig>();
    let msg = std::slice::from_raw_parts(msg, size);
    let res = bincode::decode_from_slice::<ServerMessage, _>(msg, bincode_ops());
    let (msg, _) = match res {
        Ok(msg) => msg,
        Err(e) => {
            log::error!("could not deserialize message: {}", e);
            return;
        }
    };
    match msg {
        ServerMessage::Configure { .. } => {}
        ServerMessage::Response { .. } => {}
        ServerMessage::InvokeShortcut { .. } => {}
        ServerMessage::NewInputDevice { .. } => {}
        ServerMessage::DelInputDevice { .. } => {}
        ServerMessage::ConnectorConnect { .. } => {}
        ServerMessage::ConnectorDisconnect { .. } => {}
        ServerMessage::NewConnector { .. } => {}
        ServerMessage::DelConnector { .. } => {}
        ServerMessage::TimerExpired { .. } => {}
        ServerMessage::GraphicsInitialized => {}
    }
}

#[derive(Copy, Clone)]
struct ServerData {
    srv_data: *const u8,
    srv_unref: unsafe extern "C" fn(data: *const u8),
    srv_handler: unsafe extern "C" fn(data: *const u8, msg: *const u8, size: usize),
}

pub struct TestConfig {
    srv: Cell<Option<ServerData>>,
}

impl TestConfig {
    fn send(&self, msg: ClientMessage) -> Result<(), TestError> {
        let srv = match self.srv.get() {
            Some(srv) => srv,
            _ => bail!("srv not set"),
        };
        let mut buf = vec![];
        bincode::encode_into_std_write(msg, &mut buf, bincode_ops()).unwrap();
        unsafe {
            (srv.srv_handler)(srv.srv_data, buf.as_ptr(), buf.len());
        }
        Ok(())
    }

    pub fn quit(&self) -> Result<(), TestError> {
        self.send(ClientMessage::Quit)
    }
}

impl Drop for TestConfig {
    fn drop(&mut self) {
        unsafe {
            if let Some(srv) = self.srv.take() {
                (srv.srv_unref)(srv.srv_data);
            }
        }
    }
}
