use {
    crate::{
        backend,
        client::{Client, ClientError, ClientId},
        gfx_api::GfxTexture,
        globals::{Global, GlobalName},
        ifs::{
            wl_buffer::WlBufferStorage, wl_surface::WlSurface,
            zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, zxdg_output_v1::ZxdgOutputV1,
        },
        leaks::Tracker,
        object::Object,
        rect::Rect,
        state::{ConnectorData, State},
        time::Time,
        tree::{calculate_logical_size, OutputNode},
        utils::{
            buffd::{MsgParser, MsgParserError},
            clonecell::CloneCell,
            copyhashmap::CopyHashMap,
            linkedlist::LinkedList,
            transform_ext::TransformExt,
        },
        wire::{wl_output::*, WlOutputId, ZxdgOutputV1Id},
    },
    ahash::AHashMap,
    jay_config::video::Transform,
    std::{
        cell::{Cell, RefCell},
        collections::hash_map::Entry,
        ops::Deref,
        rc::Rc,
    },
    thiserror::Error,
};

const SP_UNKNOWN: i32 = 0;
#[allow(dead_code)]
const SP_NONE: i32 = 1;
#[allow(dead_code)]
const SP_HORIZONTAL_RGB: i32 = 2;
#[allow(dead_code)]
const SP_HORIZONTAL_BGR: i32 = 3;
#[allow(dead_code)]
const SP_VERTICAL_RGB: i32 = 4;
#[allow(dead_code)]
const SP_VERTICAL_BGR: i32 = 5;

pub const TF_NORMAL: i32 = 0;
pub const TF_90: i32 = 1;
pub const TF_180: i32 = 2;
pub const TF_270: i32 = 3;
pub const TF_FLIPPED: i32 = 4;
pub const TF_FLIPPED_90: i32 = 5;
pub const TF_FLIPPED_180: i32 = 6;
pub const TF_FLIPPED_270: i32 = 7;

const MODE_CURRENT: u32 = 1;
#[allow(dead_code)]
const MODE_PREFERRED: u32 = 2;

pub struct WlOutputGlobal {
    pub name: GlobalName,
    pub state: Rc<State>,
    pub connector: Rc<ConnectorData>,
    pub pos: Cell<Rect>,
    pub output_id: Rc<OutputId>,
    pub mode: Cell<backend::Mode>,
    pub modes: Vec<backend::Mode>,
    pub node: CloneCell<Option<Rc<OutputNode>>>,
    pub width_mm: i32,
    pub height_mm: i32,
    pub bindings: RefCell<AHashMap<ClientId, AHashMap<WlOutputId, Rc<WlOutput>>>>,
    pub unused_captures: LinkedList<Rc<ZwlrScreencopyFrameV1>>,
    pub pending_captures: LinkedList<Rc<ZwlrScreencopyFrameV1>>,
    pub destroyed: Cell<bool>,
    pub legacy_scale: Cell<u32>,
    pub persistent: Rc<PersistentOutputState>,
}

pub struct PersistentOutputState {
    pub transform: Cell<Transform>,
    pub scale: Cell<crate::scale::Scale>,
    pub pos: Cell<(i32, i32)>,
}

#[derive(Eq, PartialEq, Hash)]
pub struct OutputId {
    pub connector: String,
    pub manufacturer: String,
    pub model: String,
    pub serial_number: String,
}

impl WlOutputGlobal {
    pub fn clear(&self) {
        self.node.take();
        self.bindings.borrow_mut().clear();
    }

    pub fn new(
        name: GlobalName,
        state: &Rc<State>,
        connector: &Rc<ConnectorData>,
        modes: Vec<backend::Mode>,
        mode: &backend::Mode,
        width_mm: i32,
        height_mm: i32,
        output_id: &Rc<OutputId>,
        persistent_state: &Rc<PersistentOutputState>,
    ) -> Self {
        let (x, y) = persistent_state.pos.get();
        let scale = persistent_state.scale.get();
        let (width, height) = calculate_logical_size(
            (mode.width, mode.height),
            persistent_state.transform.get(),
            scale,
        );
        Self {
            name,
            state: state.clone(),
            connector: connector.clone(),
            pos: Cell::new(Rect::new_sized(x, y, width, height).unwrap()),
            output_id: output_id.clone(),
            mode: Cell::new(*mode),
            modes,
            node: Default::default(),
            width_mm,
            height_mm,
            bindings: Default::default(),
            unused_captures: Default::default(),
            pending_captures: Default::default(),
            destroyed: Cell::new(false),
            legacy_scale: Cell::new(scale.round_up()),
            persistent: persistent_state.clone(),
        }
    }

    pub fn position(&self) -> Rect {
        self.pos.get()
    }

    pub fn for_each_binding<F: FnMut(&Rc<WlOutput>)>(&self, client: ClientId, mut f: F) {
        let bindings = self.bindings.borrow_mut();
        if let Some(bindings) = bindings.get(&client) {
            for binding in bindings.values() {
                f(binding);
            }
        }
    }

    pub fn send_enter(&self, surface: &WlSurface) {
        self.for_each_binding(surface.client.id, |b| {
            surface.send_enter(b.id);
        })
    }

    pub fn send_leave(&self, surface: &WlSurface) {
        self.for_each_binding(surface.client.id, |b| {
            surface.send_leave(b.id);
        })
    }

    pub fn send_mode(&self) {
        let bindings = self.bindings.borrow_mut();
        for binding in bindings.values() {
            for binding in binding.values() {
                binding.send_geometry();
                binding.send_mode();
                binding.send_scale();
                binding.send_done();
                let xdg = binding.xdg_outputs.lock();
                for xdg in xdg.values() {
                    xdg.send_updates();
                }
                // binding.client.flush();
            }
        }
    }

    fn bind_(
        self: Rc<Self>,
        id: WlOutputId,
        client: &Rc<Client>,
        version: u32,
    ) -> Result<(), WlOutputError> {
        let obj = Rc::new(WlOutput {
            global: self.clone(),
            id,
            xdg_outputs: Default::default(),
            client: client.clone(),
            version,
            tracker: Default::default(),
        });
        track!(client, obj);
        client.add_client_obj(&obj)?;
        self.bindings
            .borrow_mut()
            .entry(client.id)
            .or_default()
            .insert(id, obj.clone());
        obj.send_geometry();
        obj.send_mode();
        if obj.version >= SEND_SCALE_SINCE {
            obj.send_scale();
        }
        if obj.version >= SEND_NAME_SINCE {
            obj.send_name();
        }
        if obj.version >= SEND_DONE_SINCE {
            obj.send_done();
        }
        Ok(())
    }

    pub fn perform_screencopies(
        &self,
        tex: &Rc<dyn GfxTexture>,
        render_hardware_cursors: bool,
        x_off: i32,
        y_off: i32,
        size: Option<(i32, i32)>,
    ) {
        if self.pending_captures.is_empty() {
            return;
        }
        let now = Time::now().unwrap();
        let mut captures = vec![];
        for capture in self.pending_captures.iter() {
            captures.push(capture.deref().clone());
            let wl_buffer = match capture.buffer.take() {
                Some(b) => b,
                _ => {
                    log::warn!("Capture frame is pending but has no buffer attached");
                    capture.send_failed();
                    continue;
                }
            };
            if wl_buffer.destroyed() {
                capture.send_failed();
                continue;
            }
            if let Some(WlBufferStorage::Shm { mem, stride }) =
                wl_buffer.storage.borrow_mut().deref()
            {
                self.state.perform_shm_screencopy(
                    tex,
                    self.pos.get(),
                    x_off,
                    y_off,
                    size,
                    &capture,
                    mem,
                    *stride,
                    wl_buffer.format,
                    Transform::None,
                );
            } else {
                let fb = match wl_buffer.famebuffer.get() {
                    Some(fb) => fb,
                    _ => {
                        log::warn!("Capture buffer has no framebuffer");
                        capture.send_failed();
                        continue;
                    }
                };
                self.state.perform_screencopy(
                    tex,
                    &fb,
                    self.pos.get(),
                    render_hardware_cursors,
                    x_off - capture.rect.x1(),
                    y_off - capture.rect.y1(),
                    size,
                    Transform::None,
                );
            }
            if capture.with_damage.get() {
                capture.send_damage();
            }
            capture.send_ready(now.0.tv_sec as _, now.0.tv_nsec as _);
        }
        for capture in captures {
            capture.output_link.take();
        }
    }

    pub fn pixel_size(&self) -> (i32, i32) {
        let mode = self.mode.get();
        self.persistent
            .transform
            .get()
            .maybe_swap((mode.width, mode.height))
    }
}

global_base!(WlOutputGlobal, WlOutput, WlOutputError);

impl Global for WlOutputGlobal {
    fn singleton(&self) -> bool {
        false
    }

    fn version(&self) -> u32 {
        4
    }

    fn break_loops(&self) {
        self.bindings.borrow_mut().clear();
    }
}

dedicated_add_global!(WlOutputGlobal, outputs);

pub struct WlOutput {
    pub global: Rc<WlOutputGlobal>,
    pub id: WlOutputId,
    pub xdg_outputs: CopyHashMap<ZxdgOutputV1Id, Rc<ZxdgOutputV1>>,
    client: Rc<Client>,
    pub version: u32,
    tracker: Tracker<Self>,
}

pub const SEND_DONE_SINCE: u32 = 2;
pub const SEND_SCALE_SINCE: u32 = 2;
pub const SEND_NAME_SINCE: u32 = 4;

impl WlOutput {
    fn send_geometry(&self) {
        let pos = self.global.pos.get();
        let event = Geometry {
            self_id: self.id,
            x: pos.x1(),
            y: pos.y1(),
            physical_width: self.global.width_mm,
            physical_height: self.global.height_mm,
            subpixel: SP_UNKNOWN,
            make: &self.global.output_id.manufacturer,
            model: &self.global.output_id.model,
            transform: self.global.persistent.transform.get().to_wl(),
        };
        self.client.event(event);
    }

    fn send_mode(&self) {
        let mode = self.global.mode.get();
        let event = Mode {
            self_id: self.id,
            flags: MODE_CURRENT,
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_rate_millihz as _,
        };
        self.client.event(event);
    }

    fn send_scale(self: &Rc<Self>) {
        let event = Scale {
            self_id: self.id,
            factor: self.global.legacy_scale.get() as _,
        };
        self.client.event(event);
    }

    fn send_name(&self) {
        self.client.event(Name {
            self_id: self.id,
            name: &self.global.connector.name,
        });
    }

    pub fn send_done(&self) {
        let event = Done { self_id: self.id };
        self.client.event(event);
    }

    fn remove_binding(&self) {
        if let Entry::Occupied(mut e) = self.global.bindings.borrow_mut().entry(self.client.id) {
            e.get_mut().remove(&self.id);
            if e.get().is_empty() {
                e.remove();
            }
        }
    }

    fn release(&self, parser: MsgParser<'_, '_>) -> Result<(), WlOutputError> {
        let _req: Release = self.client.parse(self, parser)?;
        self.xdg_outputs.clear();
        self.remove_binding();
        self.client.remove_obj(self)?;
        Ok(())
    }
}

object_base! {
    self = WlOutput;

    RELEASE => release if self.version >= 3,
}

impl Object for WlOutput {
    fn break_loops(&self) {
        self.xdg_outputs.clear();
        self.remove_binding();
    }
}

dedicated_add_obj!(WlOutput, WlOutputId, outputs);

#[derive(Debug, Error)]
pub enum WlOutputError {
    #[error(transparent)]
    ClientError(Box<ClientError>),
    #[error("Parsing failed")]
    MsgParserError(#[source] Box<MsgParserError>),
}
efrom!(WlOutputError, ClientError);
efrom!(WlOutputError, MsgParserError);
