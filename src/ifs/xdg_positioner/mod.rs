mod types;

use crate::client::Client;
use crate::ifs::xdg_wm_base::XdgWmBaseObj;
use crate::object::{Interface, Object, ObjectId};
use crate::rect::Rect;
use crate::utils::buffd::MsgParser;
use bitflags::bitflags;
use std::cell::RefCell;
use std::rc::Rc;
pub use types::*;

const DESTROY: u32 = 0;
const SET_SIZE: u32 = 1;
const SET_ANCHOR_RECT: u32 = 2;
const SET_ANCHOR: u32 = 3;
const SET_GRAVITY: u32 = 4;
const SET_CONSTRAINT_ADJUSTMENT: u32 = 5;
const SET_OFFSET: u32 = 6;
const SET_REACTIVE: u32 = 7;
const SET_PARENT_SIZE: u32 = 8;
const SET_PARENT_CONFIGURE: u32 = 9;

const INVALID_INPUT: u32 = 0;

const NONE: u32 = 0;
const TOP: u32 = 1;
const BOTTOM: u32 = 2;
const LEFT: u32 = 3;
const RIGHT: u32 = 4;
const TOP_LEFT: u32 = 5;
const BOTTOM_LEFT: u32 = 6;
const TOP_RIGHT: u32 = 7;
const BOTTOM_RIGHT: u32 = 8;

bitflags::bitflags! {
    #[derive(Default)]
    pub struct Square: u32 {
        const TOP = 1 << 0;
        const BOTTOM = 1 << 1;
        const LEFT = 1 << 2;
        const RIGHT = 1 << 3;
    }
}

impl Square {
    fn from_enum(e: u32) -> Option<Self> {
        let s = match e {
            NONE => Square::empty(),
            TOP => Square::TOP,
            BOTTOM => Square::BOTTOM,
            LEFT => Square::LEFT,
            RIGHT => Square::RIGHT,
            TOP_LEFT => Square::TOP | Square::LEFT,
            BOTTOM_LEFT => Square::BOTTOM | Square::LEFT,
            TOP_RIGHT => Square::TOP | Square::RIGHT,
            BOTTOM_RIGHT => Square::BOTTOM | Square::RIGHT,
            _ => return None,
        };
        Some(s)
    }
}

bitflags! {
    #[derive(Default)]
    pub struct CA: u32 {
        const NONE = 0;
        const SLIDE_X = 1;
        const SLIDE_Y = 2;
        const FLIP_X = 4;
        const FLIP_Y = 8;
        const RESIZE_X = 16;
        const RESIZE_Y = 32;
    }
}

id!(XdgPositionerId);

pub struct XdgPositioner {
    id: XdgPositionerId,
    base: Rc<XdgWmBaseObj>,
    client: Rc<Client>,
    position: RefCell<XdgPositioned>,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct XdgPositioned {
    pub size_width: i32,
    pub size_height: i32,
    pub ar: Rect,
    pub anchor: Square,
    pub gravity: Square,
    pub ca: CA,
    pub off_x: i32,
    pub off_y: i32,
    pub reactive: bool,
    pub parent_width: i32,
    pub parent_height: i32,
    pub parent_serial: u32,
}

impl XdgPositioned {
    pub fn is_complete(&self) -> bool {
        self.size_height != 0 && self.size_width != 0
    }

    pub fn get_position(&self) -> Rect {
        let mut x1 = self.off_x;
        let mut y1 = self.off_x;

        if self.anchor.contains(Square::LEFT) {
            x1 += self.ar.x1();
        } else if self.anchor.contains(Square::RIGHT) {
            x1 += self.ar.x2();
        } else {
            x1 += self.ar.x1() + self.ar.width() / 2;
        }

        if self.anchor.contains(Square::TOP) {
            y1 += self.ar.y1();
        } else if self.anchor.contains(Square::BOTTOM) {
            y1 += self.ar.y2();
        } else {
            y1 += self.ar.y1() + self.ar.height() / 2;
        }

        if self.gravity.contains(Square::LEFT) {
            x1 -= self.size_width;
        } else if !self.gravity.contains(Square::RIGHT) {
            x1 -= self.size_width / 2;
        }

        if self.gravity.contains(Square::TOP) {
            y1 -= self.size_height;
        } else if !self.gravity.contains(Square::BOTTOM) {
            y1 -= self.size_height / 2;
        }

        Rect::new_sized(x1, y1, self.size_width, self.size_height).unwrap()
    }
}

impl XdgPositioner {
    pub fn new(base: &Rc<XdgWmBaseObj>, id: XdgPositionerId, client: &Rc<Client>) -> Self {
        Self {
            id,
            client: client.clone(),
            base: base.clone(),
            position: RefCell::new(Default::default()),
        }
    }

    pub fn value(&self) -> XdgPositioned {
        *self.position.borrow()
    }

    fn destroy(&self, parser: MsgParser<'_, '_>) -> Result<(), DestroyError> {
        let _req: Destroy = self.client.parse(self, parser)?;
        self.client.remove_obj(self)?;
        Ok(())
    }

    fn set_size(&self, parser: MsgParser<'_, '_>) -> Result<(), SetSizeError> {
        let req: SetSize = self.client.parse(self, parser)?;
        if req.width <= 0 || req.height <= 0 {
            self.client.protocol_error(
                self,
                INVALID_INPUT,
                format!("Cannot set a non-positive size"),
            );
            return Err(SetSizeError::NonPositiveSize);
        }
        let mut position = self.position.borrow_mut();
        position.size_width = req.width;
        position.size_height = req.height;
        Ok(())
    }

    fn set_anchor_rect(&self, parser: MsgParser<'_, '_>) -> Result<(), SetAnchorRectError> {
        let req: SetAnchorRect = self.client.parse(self, parser)?;
        if req.width < 0 || req.height < 0 {
            self.client.protocol_error(
                self,
                INVALID_INPUT,
                format!("Cannot set an anchor rect with negative size"),
            );
            return Err(SetAnchorRectError::NegativeAnchorRect);
        }
        let mut position = self.position.borrow_mut();
        position.ar = Rect::new_sized(req.x, req.y, req.width, req.height).unwrap();
        Ok(())
    }

    fn set_anchor(&self, parser: MsgParser<'_, '_>) -> Result<(), SetAnchorError> {
        let req: SetAnchor = self.client.parse(self, parser)?;
        let anchor = match Square::from_enum(req.anchor) {
            Some(a) => a,
            _ => return Err(SetAnchorError::UnknownAnchor(req.anchor)),
        };
        self.position.borrow_mut().anchor = anchor;
        Ok(())
    }

    fn set_gravity(&self, parser: MsgParser<'_, '_>) -> Result<(), SetGravityError> {
        let req: SetGravity = self.client.parse(self, parser)?;
        let gravity = match Square::from_enum(req.gravity) {
            Some(a) => a,
            _ => return Err(SetGravityError::UnknownGravity(req.gravity)),
        };
        self.position.borrow_mut().gravity = gravity;
        Ok(())
    }

    fn set_constraint_adjustment(
        &self,
        parser: MsgParser<'_, '_>,
    ) -> Result<(), SetConstraintAdjustmentError> {
        let req: SetConstraintAdjustment = self.client.parse(self, parser)?;
        let ca = match CA::from_bits(req.constraint_adjustment) {
            Some(c) => c,
            _ => {
                return Err(SetConstraintAdjustmentError::UnknownCa(
                    req.constraint_adjustment,
                ))
            }
        };
        self.position.borrow_mut().ca = ca;
        Ok(())
    }

    fn set_offset(&self, parser: MsgParser<'_, '_>) -> Result<(), SetOffsetError> {
        let req: SetOffset = self.client.parse(self, parser)?;
        let mut position = self.position.borrow_mut();
        position.off_x = req.x;
        position.off_y = req.y;
        Ok(())
    }

    fn set_reactive(&self, parser: MsgParser<'_, '_>) -> Result<(), SetReactiveError> {
        let _req: SetReactive = self.client.parse(self, parser)?;
        self.position.borrow_mut().reactive = true;
        Ok(())
    }

    fn set_parent_size(&self, parser: MsgParser<'_, '_>) -> Result<(), SetParentSizeError> {
        let req: SetParentSize = self.client.parse(self, parser)?;
        if req.parent_width < 0 || req.parent_height < 0 {
            self.client.protocol_error(
                self,
                INVALID_INPUT,
                format!("Cannot set a negative parent size"),
            );
            return Err(SetParentSizeError::NegativeParentSize);
        }
        let mut position = self.position.borrow_mut();
        position.parent_width = req.parent_width;
        position.parent_height = req.parent_height;
        Ok(())
    }

    fn set_parent_configure(
        &self,
        parser: MsgParser<'_, '_>,
    ) -> Result<(), SetParentConfigureError> {
        let req: SetParentConfigure = self.client.parse(self, parser)?;
        self.position.borrow_mut().parent_serial = req.serial;
        Ok(())
    }

    fn handle_request_(
        &self,
        request: u32,
        parser: MsgParser<'_, '_>,
    ) -> Result<(), XdgPositionerError> {
        match request {
            DESTROY => self.destroy(parser)?,
            SET_SIZE => self.set_size(parser)?,
            SET_ANCHOR_RECT => self.set_anchor_rect(parser)?,
            SET_ANCHOR => self.set_anchor(parser)?,
            SET_GRAVITY => self.set_gravity(parser)?,
            SET_CONSTRAINT_ADJUSTMENT => self.set_constraint_adjustment(parser)?,
            SET_OFFSET => self.set_offset(parser)?,
            SET_REACTIVE => self.set_reactive(parser)?,
            SET_PARENT_SIZE => self.set_parent_size(parser)?,
            SET_PARENT_CONFIGURE => self.set_parent_configure(parser)?,
            _ => unreachable!(),
        }
        Ok(())
    }
}

handle_request!(XdgPositioner);

impl Object for XdgPositioner {
    fn id(&self) -> ObjectId {
        self.id.into()
    }

    fn interface(&self) -> Interface {
        Interface::XdgPositioner
    }

    fn num_requests(&self) -> u32 {
        if self.base.version < 3 {
            SET_OFFSET + 1
        } else {
            SET_PARENT_CONFIGURE + 1
        }
    }
}
