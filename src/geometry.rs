//! The shapes a bar is drawn out of.
//!
//! These live here rather than in `config` because of where they are *used*: a `Frame`
//! carries them, and the renderer reads them off it. Nothing below `Frame` is supposed to
//! know a config exists - that is what makes the renderer replaceable - and a renderer that
//! had to reach into `config` for the name of a separator shape would have made that untrue
//! however positioned the rest of the geometry was.
//!
//! This is the whole vocabulary: a transition between two things, which way it points, and
//! how a group's corners are cut. Adding to it needs a real case rather than a hypothetical
//! one, because everything here is something a second renderer would have to draw.
//!
//! They deserialize, since a config is where a person names them, and that costs the
//! renderer nothing: it reads the enum, not the file.

use serde::Deserialize;

/// The transition drawn between two neighbouring modules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeparatorShape {
    #[default]
    None,
    Line,
    Slant,
    Chevron,
    Notch,
    Round,
    Curve,
}

impl SeparatorShape {
    pub fn is_none(self) -> bool {
        self == SeparatorShape::None
    }
}

/// Which way a separator shape points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Right,
    Left,
}

/// How a group's outer corners are cut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeShape {
    #[default]
    Round,
    None,
}

/// How both of a group's outer corners are cut, and how far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edges {
    pub left: EdgeShape,
    pub right: EdgeShape,
    pub radius: f32,
}
