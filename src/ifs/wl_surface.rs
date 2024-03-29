pub mod commit_timeline;
pub mod cursor;
pub mod ext_session_lock_surface_v1;
pub mod wl_subsurface;
pub mod wp_fractional_scale_v1;
pub mod wp_linux_drm_syncobj_surface_v1;
pub mod wp_tearing_control_v1;
pub mod wp_viewport;
pub mod x_surface;
pub mod xdg_surface;
pub mod xwayland_shell_v1;
pub mod zwlr_layer_surface_v1;
pub mod zwp_idle_inhibitor_v1;

use {
    crate::{
        backend::KeyState,
        client::{Client, ClientError, RequestParser},
        drm_feedback::DrmFeedback,
        fixed::Fixed,
        gfx_api::{AcquireSync, BufferResv, BufferResvUser, ReleaseSync, SampleRect, SyncFile},
        ifs::{
            wl_buffer::WlBuffer,
            wl_callback::WlCallback,
            wl_seat::{
                wl_pointer::PendingScroll, zwp_pointer_constraints_v1::SeatConstraint, Dnd,
                NodeSeatState, SeatId, WlSeatGlobal,
            },
            wl_surface::{
                commit_timeline::{ClearReason, CommitTimeline, CommitTimelineError},
                cursor::CursorSurface,
                wl_subsurface::{PendingSubsurfaceData, SubsurfaceId, WlSubsurface},
                wp_fractional_scale_v1::WpFractionalScaleV1,
                wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
                wp_tearing_control_v1::WpTearingControlV1,
                wp_viewport::WpViewport,
                x_surface::XSurface,
                xdg_surface::{PendingXdgSurfaceData, XdgSurfaceError},
                zwlr_layer_surface_v1::{PendingLayerSurfaceData, ZwlrLayerSurfaceV1Error},
            },
            wp_content_type_v1::ContentType,
            wp_presentation_feedback::WpPresentationFeedback,
            zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        },
        leaks::Tracker,
        object::Object,
        rect::{Rect, Region},
        renderer::Renderer,
        tree::{
            FindTreeResult, FoundNode, Node, NodeId, NodeVisitor, NodeVisitorBase, OutputNode,
            ToplevelNode,
        },
        utils::{
            buffd::{MsgParser, MsgParserError},
            cell_ext::CellExt,
            clonecell::CloneCell,
            copyhashmap::CopyHashMap,
            errorfmt::ErrorFmt,
            linkedlist::LinkedList,
            numcell::NumCell,
            smallmap::SmallMap,
            transform_ext::TransformExt,
        },
        video::{
            dmabuf::DMA_BUF_SYNC_READ,
            drm::sync_obj::{SyncObj, SyncObjPoint},
        },
        wire::{
            wl_surface::*, WlOutputId, WlSurfaceId, ZwpIdleInhibitorV1Id,
            ZwpLinuxDmabufFeedbackV1Id,
        },
        xkbcommon::ModifierState,
        xwayland::XWaylandEvent,
    },
    ahash::AHashMap,
    isnt::std_1::primitive::IsntSliceExt,
    jay_config::video::Transform,
    std::{
        cell::{Cell, RefCell},
        collections::hash_map::{Entry, OccupiedEntry},
        fmt::{Debug, Formatter},
        mem,
        ops::{Deref, DerefMut},
        rc::Rc,
    },
    thiserror::Error,
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

#[allow(dead_code)]
const INVALID_SCALE: u32 = 0;
#[allow(dead_code)]
const INVALID_TRANSFORM: u32 = 1;
#[allow(dead_code)]
const INVALID_SIZE: u32 = 2;

const OFFSET_SINCE: u32 = 5;
const BUFFER_SCALE_SINCE: u32 = 6;
const TRANSFORM_SINCE: u32 = 6;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SurfaceRole {
    None,
    Subsurface,
    XdgSurface,
    Cursor,
    DndIcon,
    ZwlrLayerSurface,
    XSurface,
    ExtSessionLockSurface,
}

impl SurfaceRole {
    fn name(self) -> &'static str {
        match self {
            SurfaceRole::None => "none",
            SurfaceRole::Subsurface => "subsurface",
            SurfaceRole::XdgSurface => "xdg_surface",
            SurfaceRole::Cursor => "cursor",
            SurfaceRole::DndIcon => "dnd_icon",
            SurfaceRole::ZwlrLayerSurface => "zwlr_layer_surface",
            SurfaceRole::XSurface => "xwayland surface",
            SurfaceRole::ExtSessionLockSurface => "ext_session_lock_surface",
        }
    }
}

pub struct SurfaceSendPreferredScaleVisitor;
impl NodeVisitorBase for SurfaceSendPreferredScaleVisitor {
    fn visit_surface(&mut self, node: &Rc<WlSurface>) {
        node.on_scale_change();
        node.node_visit_children(self);
    }
}

pub struct SurfaceSendPreferredTransformVisitor;
impl NodeVisitorBase for SurfaceSendPreferredTransformVisitor {
    fn visit_surface(&mut self, node: &Rc<WlSurface>) {
        node.send_preferred_buffer_transform();
        node.node_visit_children(self);
    }
}

struct SurfaceBufferExplicitRelease {
    sync_obj: Rc<SyncObj>,
    point: SyncObjPoint,
}

pub struct SurfaceBuffer {
    pub buffer: Rc<WlBuffer>,
    sync_files: SmallMap<BufferResvUser, SyncFile, 1>,
    pub sync: AcquireSync,
    pub release_sync: ReleaseSync,
    release: Option<SurfaceBufferExplicitRelease>,
}

impl Drop for SurfaceBuffer {
    fn drop(&mut self) {
        let sync_files = self.sync_files.take();
        if let Some(release) = &self.release {
            let Some(ctx) = self.buffer.client.state.render_ctx.get() else {
                log::error!("Cannot signal release point because there is no render context");
                return;
            };
            let ctx = ctx.sync_obj_ctx();
            if sync_files.is_not_empty() {
                let res = ctx.import_sync_files(
                    &release.sync_obj,
                    release.point,
                    sync_files.iter().map(|f| &f.1),
                );
                match res {
                    Ok(_) => return,
                    Err(e) => {
                        log::error!("Could not import sync files into sync obj: {}", ErrorFmt(e));
                    }
                }
            }
            if let Err(e) = ctx.signal(&release.sync_obj, release.point) {
                log::error!("Could not signal release point: {}", ErrorFmt(e));
            }
            return;
        }
        if let Some(dmabuf) = &self.buffer.dmabuf {
            for (_, sync_file) in &sync_files {
                if let Err(e) = dmabuf.import_sync_file(DMA_BUF_SYNC_READ, sync_file) {
                    log::error!("Could not import sync file: {}", ErrorFmt(e));
                }
            }
        }
        if !self.buffer.destroyed() {
            self.buffer.send_release();
        }
    }
}

impl Debug for SurfaceBuffer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceBuffer").finish_non_exhaustive()
    }
}

impl BufferResv for SurfaceBuffer {
    fn set_sync_file(&self, user: BufferResvUser, sync_file: &SyncFile) {
        self.sync_files.insert(user, sync_file.clone());
    }
}

pub struct WlSurface {
    pub id: WlSurfaceId,
    pub node_id: SurfaceNodeId,
    pub client: Rc<Client>,
    visible: Cell<bool>,
    role: Cell<SurfaceRole>,
    pending: RefCell<Box<PendingState>>,
    input_region: CloneCell<Option<Rc<Region>>>,
    opaque_region: Cell<Option<Rc<Region>>>,
    buffer_points: RefCell<BufferPoints>,
    pub buffer_points_norm: RefCell<SampleRect>,
    buffer_transform: Cell<Transform>,
    buffer_scale: Cell<i32>,
    src_rect: Cell<Option<[Fixed; 4]>>,
    dst_size: Cell<Option<(i32, i32)>>,
    pub extents: Cell<Rect>,
    pub buffer_abs_pos: Cell<Rect>,
    pub need_extents_update: Cell<bool>,
    pub buffer: CloneCell<Option<Rc<SurfaceBuffer>>>,
    pub buf_x: NumCell<i32>,
    pub buf_y: NumCell<i32>,
    pub children: RefCell<Option<Box<ParentData>>>,
    ext: CloneCell<Rc<dyn SurfaceExt>>,
    pub frame_requests: RefCell<Vec<Rc<WlCallback>>>,
    pub presentation_feedback: RefCell<Vec<Rc<WpPresentationFeedback>>>,
    seat_state: NodeSeatState,
    toplevel: CloneCell<Option<Rc<dyn ToplevelNode>>>,
    cursors: SmallMap<SeatId, Rc<CursorSurface>, 1>,
    dnd_icons: SmallMap<SeatId, Rc<WlSeatGlobal>, 1>,
    pub tracker: Tracker<Self>,
    idle_inhibitors: SmallMap<ZwpIdleInhibitorV1Id, Rc<ZwpIdleInhibitorV1>, 1>,
    viewporter: CloneCell<Option<Rc<WpViewport>>>,
    output: CloneCell<Rc<OutputNode>>,
    fractional_scale: CloneCell<Option<Rc<WpFractionalScaleV1>>>,
    pub constraints: SmallMap<SeatId, Rc<SeatConstraint>, 1>,
    xwayland_serial: Cell<Option<u64>>,
    tearing_control: CloneCell<Option<Rc<WpTearingControlV1>>>,
    tearing: Cell<bool>,
    version: u32,
    pub has_content_type_manager: Cell<bool>,
    content_type: Cell<Option<ContentType>>,
    pub drm_feedback: CopyHashMap<ZwpLinuxDmabufFeedbackV1Id, Rc<ZwpLinuxDmabufFeedbackV1>>,
    sync_obj_surface: CloneCell<Option<Rc<WpLinuxDrmSyncobjSurfaceV1>>>,
    destroyed: Cell<bool>,
    commit_timeline: CommitTimeline,
}

impl Debug for WlSurface {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlSurface").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct BufferPoints {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CommitAction {
    ContinueCommit,
    AbortCommit,
}

trait SurfaceExt {
    fn commit_requested(self: Rc<Self>, pending: &mut Box<PendingState>) -> CommitAction {
        let _ = pending;
        CommitAction::ContinueCommit
    }

    fn before_apply_commit(
        self: Rc<Self>,
        pending: &mut PendingState,
    ) -> Result<(), WlSurfaceError> {
        let _ = pending;
        Ok(())
    }

    fn after_apply_commit(self: Rc<Self>, pending: &mut PendingState) {
        let _ = pending;
    }

    fn is_some(&self) -> bool {
        true
    }

    fn is_none(&self) -> bool {
        !self.is_some()
    }

    fn on_surface_destroy(&self) -> Result<(), WlSurfaceError> {
        if self.is_some() {
            Err(WlSurfaceError::ReloObjectStillExists)
        } else {
            Ok(())
        }
    }

    fn update_subsurface_parent_extents(&self) {
        // nothing
    }

    fn subsurface_parent(&self) -> Option<Rc<WlSurface>> {
        None
    }

    fn extents_changed(&self) {
        // nothing
    }

    fn into_subsurface(self: Rc<Self>) -> Option<Rc<WlSubsurface>> {
        None
    }

    fn focus_node(&self) -> Option<Rc<dyn Node>> {
        None
    }

    fn into_xsurface(self: Rc<Self>) -> Option<Rc<XSurface>> {
        None
    }

    fn consume_pending_child(
        &self,
        surface: &WlSurface,
        child: SubsurfaceId,
        consume: &mut dyn FnMut(
            OccupiedEntry<SubsurfaceId, CommittedSubsurface>,
        ) -> Result<(), WlSurfaceError>,
    ) -> Result<(), WlSurfaceError> {
        surface.pending.borrow_mut().consume_child(child, consume)
    }
}

pub struct NoneSurfaceExt;

impl SurfaceExt for NoneSurfaceExt {
    fn is_some(&self) -> bool {
        false
    }
}

#[derive(Default)]
struct PendingState {
    buffer: Option<Option<Rc<WlBuffer>>>,
    offset: (i32, i32),
    opaque_region: Option<Option<Rc<Region>>>,
    input_region: Option<Option<Rc<Region>>>,
    frame_request: Vec<Rc<WlCallback>>,
    damage: bool,
    presentation_feedback: Vec<Rc<WpPresentationFeedback>>,
    src_rect: Option<Option<[Fixed; 4]>>,
    dst_size: Option<Option<(i32, i32)>>,
    scale: Option<i32>,
    transform: Option<Transform>,
    xwayland_serial: Option<u64>,
    tearing: Option<bool>,
    content_type: Option<Option<ContentType>>,
    subsurface: Option<Box<PendingSubsurfaceData>>,
    xdg_surface: Option<Box<PendingXdgSurfaceData>>,
    layer_surface: Option<Box<PendingLayerSurfaceData>>,
    subsurfaces: AHashMap<SubsurfaceId, CommittedSubsurface>,
    acquire_point: Option<(Rc<SyncObj>, SyncObjPoint)>,
    release_point: Option<(Rc<SyncObj>, SyncObjPoint)>,
    explicit_sync: bool,
}

struct CommittedSubsurface {
    subsurface: Rc<WlSubsurface>,
    state: Box<PendingState>,
}

impl PendingState {
    fn merge(&mut self, next: &mut Self, client: &Rc<Client>) {
        // discard state

        if next.buffer.is_some() {
            if let Some((sync_obj, point)) = self.release_point.take() {
                client.state.signal_point(&sync_obj, point);
            } else if let Some(Some(prev)) = self.buffer.take() {
                if !prev.destroyed() {
                    prev.send_release();
                }
            }
        }
        for fb in self.presentation_feedback.drain(..) {
            fb.send_discarded();
            let _ = client.remove_obj(&*fb);
        }

        // overwrite state

        macro_rules! opt {
            ($name:ident) => {
                if let Some(n) = next.$name.take() {
                    self.$name = Some(n);
                }
            };
        }
        opt!(buffer);
        opt!(opaque_region);
        opt!(input_region);
        opt!(src_rect);
        opt!(dst_size);
        opt!(scale);
        opt!(transform);
        opt!(xwayland_serial);
        opt!(tearing);
        opt!(content_type);
        opt!(acquire_point);
        opt!(release_point);
        {
            let (dx1, dy1) = self.offset;
            let (dx2, dy2) = mem::take(&mut next.offset);
            self.offset = (dx1 + dx2, dy1 + dy2);
        }
        self.frame_request.append(&mut next.frame_request);
        self.damage |= mem::take(&mut next.damage);
        mem::swap(
            &mut self.presentation_feedback,
            &mut next.presentation_feedback,
        );
        macro_rules! merge_ext {
            ($name:ident) => {
                if let Some(e) = &mut self.$name {
                    if let Some(n) = &mut next.$name {
                        e.merge(n);
                    }
                } else {
                    self.$name = next.$name.take();
                }
            };
        }
        merge_ext!(subsurface);
        merge_ext!(xdg_surface);
        merge_ext!(layer_surface);
        for (id, mut state) in next.subsurfaces.drain() {
            match self.subsurfaces.entry(id) {
                Entry::Occupied(mut o) => {
                    o.get_mut().state.merge(&mut state.state, client);
                }
                Entry::Vacant(v) => {
                    v.insert(state);
                }
            }
        }
    }

    fn consume_child(
        &mut self,
        child: SubsurfaceId,
        consume: impl FnOnce(
            OccupiedEntry<SubsurfaceId, CommittedSubsurface>,
        ) -> Result<(), WlSurfaceError>,
    ) -> Result<(), WlSurfaceError> {
        match self.subsurfaces.entry(child) {
            Entry::Occupied(oe) => consume(oe),
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
pub struct ParentData {
    subsurfaces: AHashMap<WlSurfaceId, Rc<WlSubsurface>>,
    pub below: LinkedList<StackElement>,
    pub above: LinkedList<StackElement>,
}

pub struct StackElement {
    pub pending: Cell<bool>,
    pub sub_surface: Rc<WlSubsurface>,
}

impl WlSurface {
    pub fn new(id: WlSurfaceId, client: &Rc<Client>, version: u32) -> Self {
        Self {
            id,
            node_id: client.state.node_ids.next(),
            client: client.clone(),
            visible: Cell::new(false),
            role: Cell::new(SurfaceRole::None),
            pending: Default::default(),
            input_region: Default::default(),
            opaque_region: Default::default(),
            buffer_points: Default::default(),
            buffer_points_norm: Default::default(),
            buffer_transform: Cell::new(Transform::None),
            buffer_scale: Cell::new(1),
            src_rect: Cell::new(None),
            dst_size: Cell::new(None),
            extents: Default::default(),
            buffer_abs_pos: Cell::new(Default::default()),
            need_extents_update: Default::default(),
            buffer: Default::default(),
            buf_x: Default::default(),
            buf_y: Default::default(),
            children: Default::default(),
            ext: CloneCell::new(client.state.none_surface_ext.clone()),
            frame_requests: Default::default(),
            presentation_feedback: Default::default(),
            seat_state: Default::default(),
            toplevel: Default::default(),
            cursors: Default::default(),
            dnd_icons: Default::default(),
            tracker: Default::default(),
            idle_inhibitors: Default::default(),
            viewporter: Default::default(),
            output: CloneCell::new(client.state.dummy_output.get().unwrap()),
            fractional_scale: Default::default(),
            constraints: Default::default(),
            xwayland_serial: Default::default(),
            tearing_control: Default::default(),
            tearing: Cell::new(false),
            version,
            has_content_type_manager: Default::default(),
            content_type: Default::default(),
            drm_feedback: Default::default(),
            sync_obj_surface: Default::default(),
            destroyed: Cell::new(false),
            commit_timeline: client.commit_timelines.create_timeline(),
        }
    }

    fn get_xsurface(self: &Rc<Self>) -> Result<Rc<XSurface>, WlSurfaceError> {
        self.set_role(SurfaceRole::XSurface)?;
        let mut ext = self.ext.get();
        if ext.is_none() {
            let xsurface = Rc::new(XSurface {
                surface: self.clone(),
                xwindow: Default::default(),
                xwayland_surface: Default::default(),
                tracker: Default::default(),
            });
            track!(self.client, xsurface);
            self.ext.set(xsurface.clone());
            ext = xsurface;
        }
        Ok(ext.into_xsurface().unwrap())
    }

    pub fn set_output(&self, output: &Rc<OutputNode>) {
        let old = self.output.set(output.clone());
        if old.id == output.id {
            return;
        }
        output.global.send_enter(self);
        old.global.send_leave(self);
        if old.global.persistent.scale.get() != output.global.persistent.scale.get() {
            self.on_scale_change();
        }
        if old.global.persistent.transform.get() != output.global.persistent.transform.get() {
            self.send_preferred_buffer_transform();
        }
        let children = self.children.borrow_mut();
        if let Some(children) = &*children {
            for ss in children.subsurfaces.values() {
                ss.surface.set_output(output);
            }
        }
    }

    fn on_scale_change(&self) {
        if let Some(fs) = self.fractional_scale.get() {
            fs.send_preferred_scale();
        }
        self.send_preferred_buffer_scale();
    }

    pub fn get_toplevel(&self) -> Option<Rc<dyn ToplevelNode>> {
        self.toplevel.get()
    }

    pub fn xwayland_serial(&self) -> Option<u64> {
        self.xwayland_serial.get()
    }

    fn set_absolute_position(&self, x1: i32, y1: i32) {
        self.buffer_abs_pos
            .set(self.buffer_abs_pos.get().at_point(x1, y1));
        if let Some(children) = self.children.borrow_mut().deref_mut() {
            for ss in children.subsurfaces.values() {
                let pos = ss.position.get();
                ss.surface
                    .set_absolute_position(x1 + pos.x1(), y1 + pos.y1());
            }
        }
    }

    pub fn add_presentation_feedback(&self, fb: &Rc<WpPresentationFeedback>) {
        self.pending
            .borrow_mut()
            .presentation_feedback
            .push(fb.clone());
    }

    pub fn is_cursor(&self) -> bool {
        self.role.get() == SurfaceRole::Cursor
    }

    pub fn get_cursor(
        self: &Rc<Self>,
        seat: &Rc<WlSeatGlobal>,
    ) -> Result<Rc<CursorSurface>, WlSurfaceError> {
        if let Some(cursor) = self.cursors.get(&seat.id()) {
            return Ok(cursor);
        }
        self.set_role(SurfaceRole::Cursor)?;
        let cursor = Rc::new(CursorSurface::new(seat, self));
        track!(self.client, cursor);
        cursor.handle_buffer_change();
        Ok(cursor)
    }

    pub fn get_focus_node(&self, seat: SeatId) -> Option<Rc<dyn Node>> {
        match self.toplevel.get() {
            Some(tl) if tl.tl_accepts_keyboard_focus() => tl.tl_focus_child(seat),
            Some(_) => None,
            _ => self.ext.get().focus_node(),
        }
    }

    pub fn send_enter(&self, output: WlOutputId) {
        self.client.event(Enter {
            self_id: self.id,
            output,
        })
    }

    pub fn send_leave(&self, output: WlOutputId) {
        self.client.event(Leave {
            self_id: self.id,
            output,
        })
    }

    pub fn send_preferred_buffer_scale(&self) {
        if self.version >= BUFFER_SCALE_SINCE {
            self.client.event(PreferredBufferScale {
                self_id: self.id,
                factor: self.output.get().global.legacy_scale.get() as _,
            });
        }
    }

    pub fn send_preferred_buffer_transform(&self) {
        if self.version >= TRANSFORM_SINCE {
            self.client.event(PreferredBufferTransform {
                self_id: self.id,
                transform: self.output.get().global.persistent.transform.get().to_wl() as _,
            });
        }
    }

    fn set_toplevel(&self, tl: Option<Rc<dyn ToplevelNode>>) {
        let ch = self.children.borrow();
        if let Some(ch) = &*ch {
            for ss in ch.subsurfaces.values() {
                ss.surface.set_toplevel(tl.clone());
            }
        }
        if self.seat_state.is_active() {
            if let Some(tl) = &tl {
                tl.tl_surface_active_changed(true);
            }
        }
        self.toplevel.set(tl);
    }

    pub fn set_role(&self, role: SurfaceRole) -> Result<(), WlSurfaceError> {
        use SurfaceRole::*;
        match (self.role.get(), role) {
            (None, _) => {}
            (old, new) if old == new => {}
            (old, new) => {
                return Err(WlSurfaceError::IncompatibleRole {
                    id: self.id,
                    old,
                    new,
                })
            }
        }
        self.role.set(role);
        Ok(())
    }

    fn unset_ext(&self) {
        self.ext.set(self.client.state.none_surface_ext.clone());
    }

    fn calculate_extents(&self) {
        let old_extents = self.extents.get();
        let mut extents = self.buffer_abs_pos.get().at_point(0, 0);
        let children = self.children.borrow();
        if let Some(children) = &*children {
            for ss in children.subsurfaces.values() {
                let ce = ss.surface.extents.get();
                if !ce.is_empty() {
                    let cp = ss.position.get();
                    let ce = ce.move_(cp.x1(), cp.y1());
                    extents = if extents.is_empty() {
                        ce
                    } else {
                        extents.union(ce)
                    };
                }
            }
        }
        self.extents.set(extents);
        self.need_extents_update.set(false);
        if old_extents != extents {
            self.ext.get().extents_changed()
        }
    }

    pub fn get_root(self: &Rc<Self>) -> Rc<WlSurface> {
        let mut root = self.clone();
        loop {
            if let Some(parent) = root.ext.get().subsurface_parent() {
                root = parent;
                continue;
            }
            break;
        }
        root
    }

    fn parse<'a, T: RequestParser<'a>>(
        &self,
        parser: MsgParser<'_, 'a>,
    ) -> Result<T, MsgParserError> {
        self.client.parse(self, parser)
    }

    fn unset_cursors(&self) {
        while let Some((_, cursor)) = self.cursors.pop() {
            cursor.handle_surface_destroy();
        }
    }

    fn unset_dnd_icons(&self) {
        while let Some((_, seat)) = self.dnd_icons.pop() {
            seat.remove_dnd_icon()
        }
    }

    fn destroy(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let _req: Destroy = self.parse(parser)?;
        self.commit_timeline.clear(ClearReason::Destroy);
        self.unset_dnd_icons();
        self.unset_cursors();
        self.ext.get().on_surface_destroy()?;
        self.destroy_node();
        {
            let mut children = self.children.borrow_mut();
            if let Some(children) = &mut *children {
                for ss in children.subsurfaces.values() {
                    ss.surface.unset_ext();
                }
            }
            *children = None;
        }
        self.buffer.set(None);
        if let Some(xwayland_serial) = self.xwayland_serial.get() {
            self.client
                .surfaces_by_xwayland_serial
                .remove(&xwayland_serial);
        }
        self.frame_requests.borrow_mut().clear();
        self.toplevel.set(None);
        self.client.remove_obj(self)?;
        self.idle_inhibitors.clear();
        self.constraints.take();
        self.destroyed.set(true);
        Ok(())
    }

    fn attach(self: &Rc<Self>, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: Attach = self.parse(parser)?;
        let pending = &mut *self.pending.borrow_mut();
        if self.version >= OFFSET_SINCE {
            if req.x != 0 || req.y != 0 {
                return Err(WlSurfaceError::OffsetInAttach);
            }
        } else {
            pending.offset = (req.x, req.y);
        }
        let buf = if req.buffer.is_some() {
            Some(self.client.lookup(req.buffer)?)
        } else {
            None
        };
        pending.buffer = Some(buf);
        Ok(())
    }

    fn damage(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let _req: Damage = self.parse(parser)?;
        self.pending.borrow_mut().damage = true;
        Ok(())
    }

    fn frame(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: Frame = self.parse(parser)?;
        let cb = Rc::new(WlCallback::new(req.callback, &self.client));
        track!(self.client, cb);
        self.client.add_client_obj(&cb)?;
        self.pending.borrow_mut().frame_request.push(cb);
        Ok(())
    }

    fn set_opaque_region(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let region: SetOpaqueRegion = self.parse(parser)?;
        let region = if region.region.is_some() {
            Some(self.client.lookup(region.region)?.region())
        } else {
            None
        };
        self.pending.borrow_mut().opaque_region = Some(region);
        Ok(())
    }

    fn set_input_region(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: SetInputRegion = self.parse(parser)?;
        let region = if req.region.is_some() {
            Some(self.client.lookup(req.region)?.region())
        } else {
            None
        };
        self.pending.borrow_mut().input_region = Some(region);
        Ok(())
    }

    fn apply_state(self: &Rc<Self>, pending: &mut PendingState) -> Result<(), WlSurfaceError> {
        for (_, mut subsurface) in pending.subsurfaces.drain() {
            subsurface
                .subsurface
                .surface
                .apply_state(&mut subsurface.state)?;
        }
        if self.destroyed.get() {
            return Ok(());
        }
        self.ext.get().before_apply_commit(pending)?;
        let mut scale_changed = false;
        if let Some(scale) = pending.scale.take() {
            scale_changed = true;
            self.buffer_scale.set(scale);
        }
        let mut buffer_transform_changed = false;
        if let Some(transform) = pending.transform.take() {
            buffer_transform_changed = true;
            self.buffer_transform.set(transform);
        }
        let mut viewport_changed = false;
        if let Some(dst_size) = pending.dst_size.take() {
            viewport_changed = true;
            self.dst_size.set(dst_size);
        }
        if let Some(src_rect) = pending.src_rect.take() {
            viewport_changed = true;
            self.src_rect.set(src_rect);
        }
        if viewport_changed {
            if let Some(rect) = self.src_rect.get() {
                if self.dst_size.is_none() {
                    if !rect[2].is_integer() || !rect[3].is_integer() {
                        return Err(WlSurfaceError::NonIntegerViewportSize);
                    }
                }
            }
        }
        let mut buffer_changed = false;
        let mut old_raw_size = None;
        let (dx, dy) = mem::take(&mut pending.offset);
        if let Some(buffer_change) = pending.buffer.take() {
            buffer_changed = true;
            if let Some(buffer) = self.buffer.take() {
                old_raw_size = Some(buffer.buffer.rect);
            }
            if let Some(buffer) = buffer_change {
                buffer.update_texture_or_log();
                let (sync, release_sync) = match pending.explicit_sync {
                    false => (AcquireSync::Implicit, ReleaseSync::Implicit),
                    true => (AcquireSync::Unnecessary, ReleaseSync::Explicit),
                };
                let release = pending
                    .release_point
                    .take()
                    .map(|(sync_obj, point)| SurfaceBufferExplicitRelease { sync_obj, point });
                let surface_buffer = SurfaceBuffer {
                    buffer,
                    sync_files: Default::default(),
                    sync,
                    release_sync,
                    release,
                };
                self.buffer.set(Some(Rc::new(surface_buffer)));
                self.buf_x.fetch_add(dx);
                self.buf_y.fetch_add(dy);
                if (dx, dy) != (0, 0) {
                    self.need_extents_update.set(true);
                    for (_, cursor) in &self.cursors {
                        cursor.dec_hotspot(dx, dy);
                    }
                }
            } else {
                self.buf_x.set(0);
                self.buf_y.set(0);
                for (_, cursor) in &self.cursors {
                    cursor.set_hotspot(0, 0);
                }
            }
        }
        let transform_changed = viewport_changed || scale_changed || buffer_transform_changed;
        if buffer_changed || transform_changed {
            let mut buffer_points = self.buffer_points.borrow_mut();
            let mut buffer_points_norm = self.buffer_points_norm.borrow_mut();
            let mut new_size = None;
            if let Some(src_rect) = self.src_rect.get() {
                if transform_changed {
                    let [mut x1, mut y1, mut width, mut height] = src_rect.map(|v| v.to_f64() as _);
                    let scale = self.buffer_scale.get();
                    if scale != 1 {
                        let scale = scale as f32;
                        x1 *= scale;
                        y1 *= scale;
                        width *= scale;
                        height *= scale;
                    }
                    *buffer_points = BufferPoints {
                        x1,
                        y1,
                        x2: x1 + width,
                        y2: y1 + height,
                    };
                }
                let size = match self.dst_size.get() {
                    Some(ds) => ds,
                    None => (src_rect[2].to_int(), src_rect[3].to_int()),
                };
                new_size = Some(size);
            } else if let Some(size) = self.dst_size.get() {
                new_size = Some(size);
            }
            if let Some(buffer) = self.buffer.get() {
                if new_size.is_none() {
                    let (mut width, mut height) = self
                        .buffer_transform
                        .get()
                        .maybe_swap(buffer.buffer.rect.size());
                    let scale = self.buffer_scale.get();
                    if scale != 1 {
                        width = (width + scale - 1) / scale;
                        height = (height + scale - 1) / scale;
                    }
                    new_size = Some((width, height));
                }
                if transform_changed || Some(buffer.buffer.rect) != old_raw_size {
                    let (x1, y1, x2, y2) = if self.src_rect.is_none() {
                        (0.0, 0.0, 1.0, 1.0)
                    } else {
                        let (width, height) = self
                            .buffer_transform
                            .get()
                            .maybe_swap(buffer.buffer.rect.size());
                        let width = width as f32;
                        let height = height as f32;
                        let x1 = buffer_points.x1 / width;
                        let x2 = buffer_points.x2 / width;
                        let y1 = buffer_points.y1 / height;
                        let y2 = buffer_points.y2 / height;
                        if x1 > 1.0 || x2 > 1.0 || y1 > 1.0 || y2 > 1.0 {
                            return Err(WlSurfaceError::ViewportOutsideBuffer);
                        }
                        (x1, y1, x2, y2)
                    };
                    *buffer_points_norm = SampleRect {
                        x1,
                        y1,
                        x2,
                        y2,
                        buffer_transform: self.buffer_transform.get(),
                    };
                }
            }
            let (width, height) = new_size.unwrap_or_default();
            if (width, height) != self.buffer_abs_pos.get().size() {
                self.need_extents_update.set(true);
            }
            self.buffer_abs_pos
                .set(self.buffer_abs_pos.get().with_size(width, height).unwrap());
        }
        self.frame_requests
            .borrow_mut()
            .extend(pending.frame_request.drain(..));
        {
            let mut fbs = self.presentation_feedback.borrow_mut();
            for fb in fbs.drain(..) {
                fb.send_discarded();
                let _ = self.client.remove_obj(&*fb);
            }
            mem::swap(fbs.deref_mut(), &mut pending.presentation_feedback);
        }
        {
            if let Some(region) = pending.input_region.take() {
                self.input_region.set(region);
            }
            if let Some(region) = pending.opaque_region.take() {
                self.opaque_region.set(region);
            }
        }
        if let Some(tearing) = pending.tearing.take() {
            self.tearing.set(tearing);
        }
        if let Some(content_type) = pending.content_type.take() {
            self.content_type.set(content_type);
        }
        if let Some(xwayland_serial) = pending.xwayland_serial.take() {
            self.xwayland_serial.set(Some(xwayland_serial));
            self.client
                .surfaces_by_xwayland_serial
                .set(xwayland_serial, self.clone());
            self.client
                .state
                .xwayland
                .queue
                .push(XWaylandEvent::SurfaceSerialAssigned(self.id));
        }
        if self.need_extents_update.get() {
            self.calculate_extents();
        }
        if buffer_changed || transform_changed {
            for (_, cursor) in &self.cursors {
                cursor.handle_buffer_change();
                cursor.update_hardware_cursor();
            }
        }
        self.ext.get().after_apply_commit(pending);
        self.client.state.damage();
        Ok(())
    }

    fn commit(self: &Rc<Self>, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let _req: Commit = self.parse(parser)?;
        let ext = self.ext.get();
        let pending = &mut *self.pending.borrow_mut();
        self.verify_explicit_sync(pending)?;
        if ext.commit_requested(pending) == CommitAction::ContinueCommit {
            self.commit_timeline.commit(self, pending)?;
        }
        Ok(())
    }

    fn verify_explicit_sync(&self, pending: &mut PendingState) -> Result<(), WlSurfaceError> {
        pending.explicit_sync = self.sync_obj_surface.is_some();
        if !pending.explicit_sync {
            return Ok(());
        }
        let have_new_buffer = match &pending.buffer {
            None => false,
            Some(b) => b.is_some(),
        };
        match (
            pending.release_point.is_some(),
            pending.acquire_point.is_some(),
            have_new_buffer,
        ) {
            (true, true, true) => Ok(()),
            (false, false, false) => Ok(()),
            (_, _, true) => Err(WlSurfaceError::MissingSyncPoints),
            (_, _, false) => Err(WlSurfaceError::UnexpectedSyncPoints),
        }
    }

    fn set_buffer_transform(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: SetBufferTransform = self.parse(parser)?;
        let Some(tf) = Transform::from_wl(req.transform) else {
            return Err(WlSurfaceError::UnknownBufferTransform(req.transform));
        };
        self.pending.borrow_mut().transform = Some(tf);
        Ok(())
    }

    fn set_buffer_scale(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: SetBufferScale = self.parse(parser)?;
        if req.scale < 1 {
            return Err(WlSurfaceError::NonPositiveBufferScale);
        }
        self.pending.borrow_mut().scale = Some(req.scale);
        Ok(())
    }

    fn damage_buffer(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let _req: DamageBuffer = self.parse(parser)?;
        self.pending.borrow_mut().damage = true;
        Ok(())
    }

    fn offset(&self, parser: MsgParser<'_, '_>) -> Result<(), WlSurfaceError> {
        let req: Offset = self.parse(parser)?;
        self.pending.borrow_mut().offset = (req.x, req.y);
        Ok(())
    }

    fn accepts_input_at(&self, x: i32, y: i32) -> bool {
        let rect = self.buffer_abs_pos.get().at_point(0, 0);
        if !rect.contains(x, y) {
            return false;
        }
        if let Some(ir) = self.input_region.get() {
            if !ir.contains(x, y) {
                return false;
            }
        }
        true
    }

    fn find_surface_at(self: &Rc<Self>, x: i32, y: i32) -> Option<(Rc<Self>, i32, i32)> {
        let children = self.children.borrow();
        let children = match children.deref() {
            Some(c) => c,
            _ => {
                return if self.accepts_input_at(x, y) {
                    Some((self.clone(), x, y))
                } else {
                    None
                };
            }
        };
        let ss = |c: &LinkedList<StackElement>| {
            for child in c.rev_iter() {
                if child.pending.get() {
                    continue;
                }
                let pos = child.sub_surface.position.get();
                if pos.contains(x, y) {
                    let (x, y) = pos.translate(x, y);
                    if let Some(res) = child.sub_surface.surface.find_surface_at(x, y) {
                        return Some(res);
                    }
                }
            }
            None
        };
        if let Some(res) = ss(&children.above) {
            return Some(res);
        }
        if self.accepts_input_at(x, y) {
            return Some((self.clone(), x, y));
        }
        if let Some(res) = ss(&children.below) {
            return Some(res);
        }
        None
    }

    fn find_tree_at_(self: &Rc<Self>, x: i32, y: i32, tree: &mut Vec<FoundNode>) -> FindTreeResult {
        match self.find_surface_at(x, y) {
            Some((node, x, y)) => {
                tree.push(FoundNode { node, x, y });
                FindTreeResult::AcceptsInput
            }
            _ => FindTreeResult::Other,
        }
    }

    fn send_seat_release_events(&self) {
        self.seat_state
            .for_each_pointer_focus(|s| s.leave_surface(self));
        self.seat_state
            .for_each_kb_focus(|s| s.unfocus_surface(self));
    }

    pub fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) == visible {
            return;
        }
        for (_, inhibitor) in &self.idle_inhibitors {
            if visible {
                inhibitor.activate();
            } else {
                inhibitor.deactivate();
            }
        }
        let children = self.children.borrow_mut();
        if let Some(children) = children.deref() {
            for child in children.subsurfaces.values() {
                child.surface.set_visible(visible);
            }
        }
        if !visible {
            self.send_seat_release_events();
        }
        self.seat_state.set_visible(self, visible);
    }

    pub fn detach_node(&self, set_invisible: bool) {
        for (_, constraint) in &self.constraints {
            constraint.deactivate();
        }
        for (_, inhibitor) in &self.idle_inhibitors {
            inhibitor.deactivate();
        }
        let children = self.children.borrow();
        if let Some(ch) = children.deref() {
            for ss in ch.subsurfaces.values() {
                ss.surface.detach_node(set_invisible);
            }
        }
        if let Some(tl) = self.toplevel.get() {
            let data = tl.tl_data();
            let mut remove = vec![];
            for (seat, s) in data.focus_node.iter() {
                if s.node_id() == self.node_id() {
                    remove.push(seat);
                }
            }
            for seat in remove {
                data.focus_node.remove(&seat);
            }
        }
        self.send_seat_release_events();
        self.seat_state.destroy_node(self);
        if self.visible.get() {
            self.client.state.damage();
        }
        if set_invisible {
            self.visible.set(false);
        }
    }

    pub fn destroy_node(&self) {
        self.detach_node(true);
    }

    pub fn set_content_type(&self, content_type: Option<ContentType>) {
        self.pending.borrow_mut().content_type = Some(content_type);
    }

    pub fn request_activation(&self) {
        if let Some(tl) = self.toplevel.get() {
            tl.tl_data().request_attention(tl.tl_as_node());
        }
    }

    pub fn send_feedback(&self, fb: &DrmFeedback) {
        for consumer in self.drm_feedback.lock().values() {
            consumer.send_feedback(fb);
        }
    }

    fn consume_pending_child(
        &self,
        child: SubsurfaceId,
        mut consume: impl FnMut(
            OccupiedEntry<SubsurfaceId, CommittedSubsurface>,
        ) -> Result<(), WlSurfaceError>,
    ) -> Result<(), WlSurfaceError> {
        self.ext
            .get()
            .consume_pending_child(self, child, &mut consume)
    }

    pub fn set_dnd_icon_seat(&self, id: SeatId, seat: Option<&Rc<WlSeatGlobal>>) {
        match seat {
            None => {
                self.dnd_icons.remove(&id);
            }
            Some(seat) => {
                self.dnd_icons.insert(id, seat.clone());
            }
        }
        self.set_visible(self.dnd_icons.is_not_empty() && self.client.state.root_visible());
    }
}

object_base! {
    self = WlSurface;

    DESTROY => destroy,
    ATTACH => attach,
    DAMAGE => damage,
    FRAME => frame,
    SET_OPAQUE_REGION => set_opaque_region,
    SET_INPUT_REGION => set_input_region,
    COMMIT => commit,
    SET_BUFFER_TRANSFORM => set_buffer_transform if self.version >= 2,
    SET_BUFFER_SCALE => set_buffer_scale if self.version >= 3,
    DAMAGE_BUFFER => damage_buffer if self.version >= 4,
    OFFSET => offset if self.version >= 5,
}

impl Object for WlSurface {
    fn break_loops(&self) {
        self.unset_dnd_icons();
        self.unset_cursors();
        self.destroy_node();
        *self.children.borrow_mut() = None;
        self.unset_ext();
        mem::take(self.frame_requests.borrow_mut().deref_mut());
        self.buffer.set(None);
        self.toplevel.set(None);
        self.idle_inhibitors.clear();
        mem::take(self.pending.borrow_mut().deref_mut());
        self.presentation_feedback.borrow_mut().clear();
        self.viewporter.take();
        self.fractional_scale.take();
        self.tearing_control.take();
        self.constraints.clear();
        self.drm_feedback.clear();
        self.commit_timeline.clear(ClearReason::BreakLoops);
    }
}

dedicated_add_obj!(WlSurface, WlSurfaceId, surfaces);

tree_id!(SurfaceNodeId);
impl Node for WlSurface {
    fn node_id(&self) -> NodeId {
        self.node_id.into()
    }

    fn node_seat_state(&self) -> &NodeSeatState {
        &self.seat_state
    }

    fn node_visit(self: Rc<Self>, visitor: &mut dyn NodeVisitor) {
        visitor.visit_surface(&self);
    }

    fn node_visit_children(&self, visitor: &mut dyn NodeVisitor) {
        let children = self.children.borrow_mut();
        if let Some(c) = children.deref() {
            for child in c.subsurfaces.values() {
                visitor.visit_surface(&child.surface);
            }
        }
    }

    fn node_visible(&self) -> bool {
        self.visible.get()
    }

    fn node_absolute_position(&self) -> Rect {
        self.buffer_abs_pos.get()
    }

    fn node_active_changed(&self, active: bool) {
        if let Some(tl) = self.toplevel.get() {
            tl.tl_surface_active_changed(active);
        }
    }

    fn node_render(&self, renderer: &mut Renderer, x: i32, y: i32, bounds: Option<&Rect>) {
        renderer.render_surface(self, x, y, bounds);
    }

    fn node_client(&self) -> Option<Rc<Client>> {
        Some(self.client.clone())
    }

    fn node_toplevel(self: Rc<Self>) -> Option<Rc<dyn ToplevelNode>> {
        self.toplevel.get()
    }

    fn node_on_key(&self, seat: &WlSeatGlobal, time_usec: u64, key: u32, state: u32) {
        seat.key_surface(self, time_usec, key, state);
    }

    fn node_on_mods(&self, seat: &WlSeatGlobal, mods: ModifierState) {
        seat.mods_surface(self, mods);
    }

    fn node_on_button(
        self: Rc<Self>,
        seat: &Rc<WlSeatGlobal>,
        time_usec: u64,
        button: u32,
        state: KeyState,
        serial: u32,
    ) {
        seat.button_surface(&self, time_usec, button, state, serial);
    }

    fn node_on_axis_event(self: Rc<Self>, seat: &Rc<WlSeatGlobal>, event: &PendingScroll) {
        seat.scroll_surface(&self, event);
    }

    fn node_on_focus(self: Rc<Self>, seat: &Rc<WlSeatGlobal>) {
        if let Some(tl) = self.toplevel.get() {
            tl.tl_data().focus_node.insert(seat.id(), self.clone());
            tl.tl_on_activate();
        }
        seat.focus_surface(&self);
    }

    fn node_on_unfocus(&self, seat: &WlSeatGlobal) {
        seat.unfocus_surface(self);
    }

    fn node_on_leave(&self, seat: &WlSeatGlobal) {
        seat.leave_surface(self);
    }

    fn node_on_pointer_enter(self: Rc<Self>, seat: &Rc<WlSeatGlobal>, x: Fixed, y: Fixed) {
        seat.enter_surface(&self, x, y)
    }

    fn node_on_pointer_motion(self: Rc<Self>, seat: &Rc<WlSeatGlobal>, x: Fixed, y: Fixed) {
        seat.motion_surface(&self, x, y)
    }

    fn node_on_pointer_relative_motion(
        &self,
        seat: &Rc<WlSeatGlobal>,
        time_usec: u64,
        dx: Fixed,
        dy: Fixed,
        dx_unaccelerated: Fixed,
        dy_unaccelerated: Fixed,
    ) {
        seat.relative_motion_surface(self, time_usec, dx, dy, dx_unaccelerated, dy_unaccelerated);
    }

    fn node_on_dnd_drop(&self, dnd: &Dnd) {
        dnd.seat.dnd_surface_drop(self, dnd);
    }

    fn node_on_dnd_leave(&self, dnd: &Dnd) {
        dnd.seat.dnd_surface_leave(self, dnd);
    }

    fn node_on_dnd_enter(&self, dnd: &Dnd, x: Fixed, y: Fixed, serial: u32) {
        dnd.seat.dnd_surface_enter(self, dnd, x, y, serial);
    }

    fn node_on_dnd_motion(&self, dnd: &Dnd, time_usec: u64, x: Fixed, y: Fixed) {
        dnd.seat.dnd_surface_motion(self, dnd, time_usec, x, y);
    }

    fn node_into_surface(self: Rc<Self>) -> Option<Rc<WlSurface>> {
        Some(self.clone())
    }

    fn node_is_xwayland_surface(&self) -> bool {
        self.client.is_xwayland
    }
}

#[derive(Debug, Error)]
pub enum WlSurfaceError {
    #[error(transparent)]
    ClientError(Box<ClientError>),
    #[error(transparent)]
    ZwlrLayerSurfaceV1Error(Box<ZwlrLayerSurfaceV1Error>),
    #[error(transparent)]
    XdgSurfaceError(Box<XdgSurfaceError>),
    #[error("Surface {} cannot be assigned the role {} because it already has the role {}", .id, .new.name(), .old.name())]
    IncompatibleRole {
        id: WlSurfaceId,
        old: SurfaceRole,
        new: SurfaceRole,
    },
    #[error("Cannot destroy a `wl_surface` before its role object")]
    ReloObjectStillExists,
    #[error("Parsing failed")]
    MsgParserError(#[source] Box<MsgParserError>),
    #[error("Buffer scale is not positive")]
    NonPositiveBufferScale,
    #[error("Unknown buffer transform {0}")]
    UnknownBufferTransform(i32),
    #[error("Viewport source is not integer-sized and destination size is not set")]
    NonIntegerViewportSize,
    #[error("Viewport source is not contained in the attached buffer")]
    ViewportOutsideBuffer,
    #[error("attach request must not contain offset")]
    OffsetInAttach,
    #[error(transparent)]
    CommitTimelineError(Box<CommitTimelineError>),
    #[error("Explicit sync buffer is attached but acquire or release points are not set")]
    MissingSyncPoints,
    #[error("No buffer is attached but acquire or release point is set")]
    UnexpectedSyncPoints,
}
efrom!(WlSurfaceError, ClientError);
efrom!(WlSurfaceError, XdgSurfaceError);
efrom!(WlSurfaceError, ZwlrLayerSurfaceV1Error);
efrom!(WlSurfaceError, MsgParserError);
efrom!(WlSurfaceError, CommitTimelineError);
