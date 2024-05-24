use {
    crate::{
        cursor::Cursor,
        fixed::Fixed,
        format::Format,
        rect::Rect,
        renderer::{renderer_base::RendererBase, RenderResult, Renderer},
        scale::Scale,
        state::State,
        theme::Color,
        tree::{Node, OutputNode},
        utils::{clonecell::UnsafeCellCloneSafe, transform_ext::TransformExt},
        video::{dmabuf::DmaBuf, drm::sync_obj::SyncObjCtx, gbm::GbmDevice, Modifier},
    },
    ahash::AHashMap,
    indexmap::IndexSet,
    jay_config::video::{GfxApi, Transform},
    std::{
        any::Any,
        cell::Cell,
        error::Error,
        ffi::CString,
        fmt::{Debug, Formatter},
        ops::Deref,
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering::Relaxed},
    },
    thiserror::Error,
    uapi::OwnedFd,
};

pub enum GfxApiOpt {
    Sync,
    FillRect(FillRect),
    CopyTexture(CopyTexture),
}

pub struct GfxRenderPass {
    pub ops: Vec<GfxApiOpt>,
    pub clear: Option<Color>,
}

#[derive(Default, Debug, Copy, Clone, PartialEq)]
pub struct SampleRect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub buffer_transform: Transform,
}

impl SampleRect {
    pub fn identity() -> Self {
        Self {
            x1: 0.0,
            y1: 0.0,
            x2: 1.0,
            y2: 1.0,
            buffer_transform: Transform::None,
        }
    }

    pub fn is_covering(&self) -> bool {
        self.x1 == 0.0 && self.y1 == 0.0 && self.x2 == 1.0 && self.y2 == 1.0
    }

    pub fn to_points(&self) -> [[f32; 2]; 4] {
        use Transform::*;
        let x1 = self.x1;
        let x2 = self.x2;
        let y1 = self.y1;
        let y2 = self.y2;
        match self.buffer_transform {
            None => [[x2, y1], [x1, y1], [x2, y2], [x1, y2]],
            Rotate90 => [[y1, x1], [y1, x2], [y2, x1], [y2, x2]],
            Rotate180 => [[x1, y2], [x2, y2], [x1, y1], [x2, y1]],
            Rotate270 => [[y2, x2], [y2, x1], [y1, x2], [y1, x1]],
            Flip => [[x1, y1], [x2, y1], [x1, y2], [x2, y2]],
            FlipRotate90 => [[y1, x2], [y1, x1], [y2, x2], [y2, x1]],
            FlipRotate180 => [[x2, y2], [x1, y2], [x2, y1], [x1, y1]],
            FlipRotate270 => [[y2, x1], [y2, x2], [y1, x1], [y1, x2]],
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct FramebufferRect {
    pub x1: f32,
    pub x2: f32,
    pub y1: f32,
    pub y2: f32,
    pub output_transform: Transform,
}

impl FramebufferRect {
    pub fn new(
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        transform: Transform,
        width: f32,
        height: f32,
    ) -> Self {
        Self {
            x1: 2.0 * x1 / width - 1.0,
            x2: 2.0 * x2 / width - 1.0,
            y1: 2.0 * y1 / height - 1.0,
            y2: 2.0 * y2 / height - 1.0,
            output_transform: transform,
        }
    }

    pub fn to_points(&self) -> [[f32; 2]; 4] {
        use Transform::*;
        let x1 = self.x1;
        let x2 = self.x2;
        let y1 = self.y1;
        let y2 = self.y2;
        match self.output_transform {
            None => [[x2, y1], [x1, y1], [x2, y2], [x1, y2]],
            Rotate90 => [[y1, -x2], [y1, -x1], [y2, -x2], [y2, -x1]],
            Rotate180 => [[-x2, -y1], [-x1, -y1], [-x2, -y2], [-x1, -y2]],
            Rotate270 => [[-y1, x2], [-y1, x1], [-y2, x2], [-y2, x1]],
            Flip => [[-x2, y1], [-x1, y1], [-x2, y2], [-x1, y2]],
            FlipRotate90 => [[y1, x2], [y1, x1], [y2, x2], [y2, x1]],
            FlipRotate180 => [[x2, -y1], [x1, -y1], [x2, -y2], [x1, -y2]],
            FlipRotate270 => [[-y1, -x2], [-y1, -x1], [-y2, -x2], [-y2, -x1]],
        }
    }

    pub fn is_covering(&self) -> bool {
        self.x1 == -1.0 && self.y1 == -1.0 && self.x2 == 1.0 && self.y2 == 1.0
    }
}

#[derive(Debug)]
pub struct FillRect {
    pub rect: FramebufferRect,
    pub color: Color,
}

pub struct CopyTexture {
    pub tex: Rc<dyn GfxTexture>,
    pub source: SampleRect,
    pub target: FramebufferRect,
    pub buffer_resv: Option<Rc<dyn BufferResv>>,
    pub acquire_sync: AcquireSync,
    pub release_sync: ReleaseSync,
    pub alpha: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct SyncFile(pub Rc<OwnedFd>);

impl Deref for SyncFile {
    type Target = Rc<OwnedFd>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

unsafe impl UnsafeCellCloneSafe for SyncFile {}

#[derive(Clone)]
pub enum AcquireSync {
    None,
    Implicit,
    SyncFile { sync_file: SyncFile },
    Unnecessary,
}

impl AcquireSync {
    pub fn from_sync_file(sync_file: Option<SyncFile>) -> Self {
        match sync_file {
            None => Self::Implicit,
            Some(sync_file) => Self::SyncFile { sync_file },
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ReleaseSync {
    None,
    Implicit,
    Explicit,
}

impl Debug for AcquireSync {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            AcquireSync::None => "None",
            AcquireSync::Implicit => "Implicit",
            AcquireSync::SyncFile { .. } => "SyncFile",
            AcquireSync::Unnecessary => "Unnecessary",
        };
        f.debug_struct(name).finish_non_exhaustive()
    }
}

pub trait BufferResv: Debug {
    fn set_sync_file(&self, user: BufferResvUser, sync_file: &SyncFile);
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BufferResvUser(u64);

impl Default for BufferResvUser {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Relaxed))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResetStatus {
    Guilty,
    Innocent,
    Unknown,
    Other(u32),
}

pub trait GfxFramebuffer: Debug {
    fn take_render_ops(&self) -> Vec<GfxApiOpt>;

    fn physical_size(&self) -> (i32, i32);

    fn render(
        &self,
        ops: Vec<GfxApiOpt>,
        clear: Option<&Color>,
    ) -> Result<Option<SyncFile>, GfxError>;

    fn copy_to_shm(
        self: Rc<Self>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: &'static Format,
        shm: &[Cell<u8>],
    ) -> Result<(), GfxError>;

    fn format(&self) -> &'static Format;
}

impl dyn GfxFramebuffer {
    pub fn clear(&self) -> Result<Option<SyncFile>, GfxError> {
        self.clear_with(0.0, 0.0, 0.0, 0.0)
    }

    pub fn clear_with(&self, r: f32, g: f32, b: f32, a: f32) -> Result<Option<SyncFile>, GfxError> {
        let ops = self.take_render_ops();
        self.render(ops, Some(&Color { r, g, b, a }))
    }

    pub fn logical_size(&self, transform: Transform) -> (i32, i32) {
        transform.maybe_swap(self.physical_size())
    }

    pub fn renderer_base<'a>(
        &self,
        ops: &'a mut Vec<GfxApiOpt>,
        scale: Scale,
        transform: Transform,
    ) -> RendererBase<'a> {
        let (width, height) = self.logical_size(transform);
        RendererBase {
            ops,
            scaled: scale != 1,
            scale,
            scalef: scale.to_f64(),
            transform,
            fb_width: width as _,
            fb_height: height as _,
        }
    }

    pub fn copy_texture(
        &self,
        texture: &Rc<dyn GfxTexture>,
        acquire_sync: AcquireSync,
        release_sync: ReleaseSync,
        x: i32,
        y: i32,
    ) -> Result<Option<SyncFile>, GfxError> {
        let mut ops = self.take_render_ops();
        let scale = Scale::from_int(1);
        let mut renderer = self.renderer_base(&mut ops, scale, Transform::None);
        renderer.render_texture(
            texture,
            None,
            x,
            y,
            None,
            None,
            scale,
            None,
            None,
            acquire_sync,
            release_sync,
        );
        let clear = self.format().has_alpha.then_some(&Color::TRANSPARENT);
        self.render(ops, clear)
    }

    pub fn render_custom(
        &self,
        scale: Scale,
        clear: Option<&Color>,
        f: &mut dyn FnMut(&mut RendererBase),
    ) -> Result<Option<SyncFile>, GfxError> {
        let mut ops = self.take_render_ops();
        let mut renderer = self.renderer_base(&mut ops, scale, Transform::None);
        f(&mut renderer);
        self.render(ops, clear)
    }

    pub fn create_render_pass(
        &self,
        node: &dyn Node,
        state: &State,
        cursor_rect: Option<Rect>,
        result: Option<&mut RenderResult>,
        scale: Scale,
        render_cursor: bool,
        render_hardware_cursor: bool,
        black_background: bool,
        transform: Transform,
    ) -> GfxRenderPass {
        let mut ops = self.take_render_ops();
        let mut renderer = Renderer {
            base: self.renderer_base(&mut ops, scale, transform),
            state,
            result,
            logical_extents: node.node_absolute_position().at_point(0, 0),
            pixel_extents: {
                let (width, height) = self.logical_size(transform);
                Rect::new(0, 0, width, height).unwrap()
            },
        };
        node.node_render(&mut renderer, 0, 0, None);
        if let Some(rect) = cursor_rect {
            let seats = state.globals.lock_seats();
            for seat in seats.values() {
                let (x, y) = seat.pointer_cursor().position();
                if let Some(im) = seat.input_method() {
                    for (_, popup) in &im.popups {
                        if popup.surface.node_visible() {
                            let pos = popup.surface.buffer_abs_pos.get();
                            let extents = popup.surface.extents.get().move_(pos.x1(), pos.y1());
                            if extents.intersects(&rect) {
                                let (x, y) = rect.translate(pos.x1(), pos.y1());
                                renderer.render_surface(&popup.surface, x, y, None);
                            }
                        }
                    }
                }
                if let Some(drag) = seat.toplevel_drag() {
                    if let Some(tl) = drag.toplevel.get() {
                        if tl.xdg.surface.buffer.get().is_some() {
                            let (x, y) = rect.translate(
                                x.round_down() - drag.x_off.get(),
                                y.round_down() - drag.y_off.get(),
                            );
                            renderer.render_xdg_surface(&tl.xdg, x, y, None)
                        }
                    }
                }
                if let Some(dnd_icon) = seat.dnd_icon() {
                    let extents = dnd_icon.extents.get().move_(
                        x.round_down() + dnd_icon.buf_x.get(),
                        y.round_down() + dnd_icon.buf_y.get(),
                    );
                    if extents.intersects(&rect) {
                        let (x, y) = rect.translate(extents.x1(), extents.y1());
                        renderer.render_surface(&dnd_icon, x, y, None);
                    }
                }
                if render_cursor {
                    let cursor_user_group = seat.cursor_group();
                    if render_hardware_cursor || !cursor_user_group.hardware_cursor() {
                        if let Some(cursor_user) = cursor_user_group.active() {
                            if let Some(cursor) = cursor_user.get() {
                                cursor.tick();
                                let (mut x, mut y) = cursor_user.position();
                                x -= Fixed::from_int(rect.x1());
                                y -= Fixed::from_int(rect.y1());
                                cursor.render(&mut renderer, x, y);
                            }
                        }
                    }
                }
            }
        }
        let c = match black_background {
            true => Color::SOLID_BLACK,
            false => state.theme.colors.background.get(),
        };
        GfxRenderPass {
            ops,
            clear: Some(c),
        }
    }

    pub fn perform_render_pass(&self, pass: GfxRenderPass) -> Result<Option<SyncFile>, GfxError> {
        self.render(pass.ops, pass.clear.as_ref())
    }

    pub fn render_output(
        &self,
        node: &OutputNode,
        state: &State,
        cursor_rect: Option<Rect>,
        result: Option<&mut RenderResult>,
        scale: Scale,
        render_hardware_cursor: bool,
    ) -> Result<Option<SyncFile>, GfxError> {
        self.render_node(
            node,
            state,
            cursor_rect,
            result,
            scale,
            true,
            render_hardware_cursor,
            node.has_fullscreen(),
            node.global.persistent.transform.get(),
        )
    }

    pub fn render_node(
        &self,
        node: &dyn Node,
        state: &State,
        cursor_rect: Option<Rect>,
        result: Option<&mut RenderResult>,
        scale: Scale,
        render_cursor: bool,
        render_hardware_cursor: bool,
        black_background: bool,
        transform: Transform,
    ) -> Result<Option<SyncFile>, GfxError> {
        let pass = self.create_render_pass(
            node,
            state,
            cursor_rect,
            result,
            scale,
            render_cursor,
            render_hardware_cursor,
            black_background,
            transform,
        );
        self.perform_render_pass(pass)
    }

    pub fn render_hardware_cursor(
        &self,
        cursor: &dyn Cursor,
        state: &State,
        scale: Scale,
        transform: Transform,
    ) -> Result<Option<SyncFile>, GfxError> {
        let mut ops = self.take_render_ops();
        let mut renderer = Renderer {
            base: self.renderer_base(&mut ops, scale, transform),
            state,
            result: None,
            logical_extents: Rect::new_empty(0, 0),
            pixel_extents: {
                let (width, height) = self.logical_size(transform);
                Rect::new(0, 0, width, height).unwrap()
            },
        };
        cursor.render_hardware_cursor(&mut renderer);
        self.render(ops, Some(&Color::TRANSPARENT))
    }
}

pub trait GfxImage {
    fn to_framebuffer(self: Rc<Self>) -> Result<Rc<dyn GfxFramebuffer>, GfxError>;

    fn to_texture(self: Rc<Self>) -> Result<Rc<dyn GfxTexture>, GfxError>;

    fn width(&self) -> i32;
    fn height(&self) -> i32;
}

pub trait GfxTexture: Debug {
    fn size(&self) -> (i32, i32);
    fn as_any(&self) -> &dyn Any;
    fn into_any(self: Rc<Self>) -> Rc<dyn Any>;
    fn read_pixels(
        self: Rc<Self>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: &'static Format,
        shm: &[Cell<u8>],
    ) -> Result<(), GfxError>;
    fn dmabuf(&self) -> Option<&DmaBuf>;
    fn format(&self) -> &'static Format;
}

pub trait GfxContext: Debug {
    fn reset_status(&self) -> Option<ResetStatus>;

    fn render_node(&self) -> Rc<CString>;

    fn formats(&self) -> Rc<AHashMap<u32, GfxFormat>>;

    fn dmabuf_fb(self: Rc<Self>, buf: &DmaBuf) -> Result<Rc<dyn GfxFramebuffer>, GfxError> {
        self.dmabuf_img(buf)?.to_framebuffer()
    }

    fn dmabuf_img(self: Rc<Self>, buf: &DmaBuf) -> Result<Rc<dyn GfxImage>, GfxError>;

    fn shmem_texture(
        self: Rc<Self>,
        old: Option<Rc<dyn GfxTexture>>,
        data: &[Cell<u8>],
        format: &'static Format,
        width: i32,
        height: i32,
        stride: i32,
        damage: Option<&[Rect]>,
    ) -> Result<Rc<dyn GfxTexture>, GfxError>;

    fn gbm(&self) -> &GbmDevice;

    fn gfx_api(&self) -> GfxApi;

    fn create_fb(
        self: Rc<Self>,
        width: i32,
        height: i32,
        stride: i32,
        format: &'static Format,
    ) -> Result<Rc<dyn GfxFramebuffer>, GfxError>;

    fn sync_obj_ctx(&self) -> &Rc<SyncObjCtx>;
}

#[derive(Debug)]
pub struct GfxFormat {
    pub format: &'static Format,
    pub read_modifiers: IndexSet<Modifier>,
    pub write_modifiers: IndexSet<Modifier>,
}

#[derive(Error)]
#[error(transparent)]
pub struct GfxError(pub Box<dyn Error>);

impl Debug for GfxError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}
