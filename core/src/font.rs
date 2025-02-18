use crate::html::TextSpan;
use crate::prelude::*;
use crate::string::WStr;
use gc_arena::{Collect, Gc, Mutation};
use ruffle_render::backend::null::NullBitmapSource;
use ruffle_render::backend::{RenderBackend, ShapeHandle};
use ruffle_render::transform::Transform;
use std::cell::{Ref, RefCell};
use std::cmp::max;

pub use swf::TextGridFit;

/// Certain Flash routines measure text by rounding down to the nearest whole pixel.
pub fn round_down_to_pixel(t: Twips) -> Twips {
    Twips::from_pixels(t.to_pixels().floor())
}

/// Parameters necessary to evaluate a font.
#[derive(Copy, Clone, Debug)]
pub struct EvalParameters {
    /// The height of each glyph, equivalent to a font size.
    height: Twips,

    /// Additional letter spacing to be added to or removed from each glyph
    /// after normal or kerned glyph advances are applied.
    letter_spacing: Twips,

    /// Whether or not to allow use of font-provided kerning metrics.
    ///
    /// Fonts can optionally add or remove additional spacing between specific
    /// pairs of letters, separate from the ordinary width between glyphs. This
    /// parameter allows enabling or disabling that feature.
    kerning: bool,
}

impl EvalParameters {
    /// Construct eval parameters from their individual parts.
    #[allow(dead_code)]
    fn from_parts(height: Twips, letter_spacing: Twips, kerning: bool) -> Self {
        Self {
            height,
            letter_spacing,
            kerning,
        }
    }

    /// Convert the formatting on a text span over to font evaluation
    /// parameters.
    pub fn from_span(span: &TextSpan) -> Self {
        Self {
            height: Twips::from_pixels(span.size),
            letter_spacing: Twips::from_pixels(span.letter_spacing),
            kerning: span.kerning,
        }
    }

    /// Get the height that the font would be evaluated at.
    pub fn height(&self) -> Twips {
        self.height
    }
}

#[derive(Debug, Clone, Collect, Copy)]
#[collect(no_drop)]
pub struct Font<'gc>(Gc<'gc, FontData>);

#[derive(Debug, Clone, Collect)]
#[collect(require_static)]
struct FontData {
    /// The list of glyphs defined in the font.
    /// Used directly by `DefineText` tags.
    glyphs: Vec<Glyph>,

    /// A map from a Unicode code point to glyph in the `glyphs` array.
    /// Used by `DefineEditText` tags.
    code_point_to_glyph: fnv::FnvHashMap<u16, usize>,

    /// The scaling applied to the font height to render at the proper size.
    /// This depends on the DefineFont tag version.
    scale: f32,

    /// Kerning infomration.
    /// Maps from a pair of unicode code points to horizontal offset value.
    kerning_pairs: fnv::FnvHashMap<(u16, u16), Twips>,

    /// The distance from the top of each glyph to the baseline of the font, in
    /// EM-square coordinates.
    ascent: u16,

    /// The distance from the baseline of the font to the bottom of each glyph,
    /// in EM-square coordinates.
    descent: u16,

    /// The distance between the bottom of any one glyph and the top of
    /// another, in EM-square coordinates.
    leading: i16,

    /// The identity of the font.
    #[collect(require_static)]
    descriptor: FontDescriptor,
}

impl<'gc> Font<'gc> {
    pub fn from_swf_tag(
        gc_context: &Mutation<'gc>,
        renderer: &mut dyn RenderBackend,
        tag: swf::Font,
        encoding: &'static swf::Encoding,
    ) -> Font<'gc> {
        let mut code_point_to_glyph = fnv::FnvHashMap::default();

        let descriptor = FontDescriptor::from_swf_tag(&tag, encoding);
        let (ascent, descent, leading) = if let Some(layout) = &tag.layout {
            (layout.ascent, layout.descent, layout.leading)
        } else {
            (0, 0, 0)
        };

        let glyphs = tag
            .glyphs
            .into_iter()
            .enumerate()
            .map(|(index, swf_glyph)| {
                let code = swf_glyph.code;
                code_point_to_glyph.insert(code, index);

                let glyph = Glyph {
                    shape_handle: None.into(),
                    shape: None.into(),
                    swf_glyph,
                };

                // Eager-load ASCII characters.
                if code < 128 {
                    glyph.shape_handle(renderer);
                }

                glyph
            })
            .collect();

        let kerning_pairs: fnv::FnvHashMap<(u16, u16), Twips> = if let Some(layout) = &tag.layout {
            layout
                .kerning
                .iter()
                .map(|kerning| ((kerning.left_code, kerning.right_code), kerning.adjustment))
                .collect()
        } else {
            fnv::FnvHashMap::default()
        };

        Font(Gc::new(
            gc_context,
            FontData {
                glyphs,
                code_point_to_glyph,

                /// DefineFont3 stores coordinates at 20x the scale of DefineFont1/2.
                /// (SWF19 p.164)
                scale: if tag.version >= 3 { 20480.0 } else { 1024.0 },
                kerning_pairs,
                ascent,
                descent,
                leading,
                descriptor,
            },
        ))
    }

    /// Returns whether this font contains glyph shapes.
    /// If not, this font should be rendered as a device font.
    pub fn has_glyphs(&self) -> bool {
        !self.0.glyphs.is_empty()
    }

    /// Returns a glyph entry by index.
    /// Used by `Text` display objects.
    pub fn get_glyph(&self, i: usize) -> Option<&Glyph> {
        self.0.glyphs.get(i)
    }

    /// Returns a glyph entry by character.
    /// Used by `EditText` display objects.
    pub fn get_glyph_for_char(&self, c: char) -> Option<&Glyph> {
        // TODO: Properly handle UTF-16/out-of-bounds code points.
        let code_point = c as u16;
        if let Some(index) = self.0.code_point_to_glyph.get(&code_point) {
            self.get_glyph(*index)
        } else {
            None
        }
    }

    /// Determine if this font contains all the glyphs within a given string.
    pub fn has_glyphs_for_str(&self, target_str: &WStr) -> bool {
        for character in target_str.chars() {
            let c = character.unwrap_or(char::REPLACEMENT_CHARACTER);
            if self.get_glyph_for_char(c).is_none() {
                return false;
            }
        }

        true
    }

    /// Given a pair of characters, applies the offset that should be applied
    /// to the advance value between these two characters.
    /// Returns 0 twips if no kerning offset exists between these two characters.
    pub fn get_kerning_offset(&self, left: char, right: char) -> Twips {
        // TODO: Properly handle UTF-16/out-of-bounds code points.
        let left_code_point = left as u16;
        let right_code_point = right as u16;
        self.0
            .kerning_pairs
            .get(&(left_code_point, right_code_point))
            .cloned()
            .unwrap_or_default()
    }

    /// Return the leading for this font at a given height.
    pub fn get_leading_for_height(&self, height: Twips) -> Twips {
        let scale = height.get() as f32 / self.scale();

        Twips::new((self.0.leading as f32 * scale) as i32)
    }

    /// Get the baseline from the top of the glyph at a given height.
    pub fn get_baseline_for_height(&self, height: Twips) -> Twips {
        let scale = height.get() as f32 / self.scale();

        Twips::new((self.0.ascent as f32 * scale) as i32)
    }

    /// Get the descent from the baseline to the bottom of the glyph at a given height.
    pub fn get_descent_for_height(&self, height: Twips) -> Twips {
        let scale = height.get() as f32 / self.scale();

        Twips::new((self.0.descent as f32 * scale) as i32)
    }

    /// Returns whether this font contains kerning information.
    pub fn has_kerning_info(&self) -> bool {
        !self.0.kerning_pairs.is_empty()
    }

    pub fn scale(&self) -> f32 {
        self.0.scale
    }

    /// Evaluate this font against a particular string on a glyph-by-glyph
    /// basis.
    ///
    /// This function takes the text string to evaluate against, the base
    /// transform to start from, the height of each glyph, and produces a list
    /// of transforms and glyphs which will be consumed by the `glyph_func`
    /// closure. This corresponds to the series of drawing operations necessary
    /// to render the text on a single horizontal line.
    pub fn evaluate<FGlyph>(
        &self,
        text: &WStr, // TODO: take an `IntoIterator<Item=char>`, to not depend on string representation?
        mut transform: Transform,
        params: EvalParameters,
        mut glyph_func: FGlyph,
    ) where
        FGlyph: FnMut(usize, &Transform, &Glyph, Twips, Twips),
    {
        transform.matrix.ty += params.height;
        let scale = params.height.get() as f32 / self.scale();

        transform.matrix.a = scale;
        transform.matrix.d = scale;
        let mut char_indices = text.char_indices().peekable();
        let has_kerning_info = self.has_kerning_info();
        let mut x = Twips::ZERO;
        while let Some((pos, c)) = char_indices.next() {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            if let Some(glyph) = self.get_glyph_for_char(c) {
                let mut advance = Twips::new(glyph.swf_glyph.advance.into());
                if has_kerning_info && params.kerning {
                    let next_char = char_indices.peek().cloned().unwrap_or((0, Ok('\0'))).1;
                    let next_char = next_char.unwrap_or(char::REPLACEMENT_CHARACTER);
                    advance += self.get_kerning_offset(c, next_char);
                }
                let twips_advance =
                    Twips::new((advance.get() as f32 * scale) as i32) + params.letter_spacing;

                glyph_func(pos, &transform, glyph, twips_advance, x);

                // Step horizontally.
                transform.matrix.tx += twips_advance;
                x += twips_advance;
            }
        }
    }

    /// Measure a particular string's metrics (width and height).
    ///
    /// The `round` flag causes the returned coordinates to be rounded down to
    /// the nearest pixel.
    pub fn measure(&self, text: &WStr, params: EvalParameters, round: bool) -> (Twips, Twips) {
        let mut width = Twips::ZERO;
        let mut height = Twips::ZERO;

        self.evaluate(
            text,
            Default::default(),
            params,
            |_pos, transform, _glyph, advance, _x| {
                let tx = transform.matrix.tx;
                let ty = transform.matrix.ty;

                if round {
                    width = width.max(round_down_to_pixel(tx + advance));
                    height = height.max(round_down_to_pixel(ty));
                } else {
                    width = width.max(tx + advance);
                    height = height.max(ty);
                }
            },
        );

        if text.is_empty() {
            height = max(height, params.height);
        }

        (width, height)
    }

    /// Given a line of text, find the first breakpoint within the text.
    ///
    /// This function assumes only `" "` is valid whitespace to split words on,
    /// and will not attempt to break words that are longer than `width`, nor
    /// will it break at newlines.
    ///
    /// The given `offset` determines the start of the initial line, while the
    /// `width` indicates how long the line is supposed to be. Be careful to
    /// note that it is possible for this function to return `0`; that
    /// indicates that the string itself cannot fit on the line and should
    /// break onto the next one.
    ///
    /// This function yields `None` if the line is not broken.
    ///
    /// TODO: This function and, more generally, this entire file will need to
    /// be internationalized to implement AS3 `flash.text.engine`.
    pub fn wrap_line(
        &self,
        text: &WStr,
        params: EvalParameters,
        width: Twips,
        offset: Twips,
        mut is_start_of_line: bool,
    ) -> Option<usize> {
        let mut remaining_width = width - offset;
        if remaining_width < Twips::from_pixels(0.0) {
            return Some(0);
        }

        let mut line_end = 0;

        for word in text.split(b' ') {
            let word_start = word.offset_in(text).unwrap();
            let word_end = word_start + word.len();

            let measure = self.measure(
                // +1 is fine because ' ' is 1 unit
                text.slice(word_start..word_end + 1).unwrap_or(word),
                params,
                false,
            );

            if is_start_of_line && measure.0 > remaining_width {
                //Failsafe for if we get a word wider than the field.
                let mut last_passing_breakpoint = (Twips::ZERO, Twips::ZERO);

                let cur_slice = &text[word_start..];
                let mut char_iter = cur_slice.char_indices();
                let mut prev_char_index = word_start;
                let mut prev_frag_end = 0;

                char_iter.next(); // No need to check cur_slice[0..0]
                while last_passing_breakpoint.0 < remaining_width {
                    prev_char_index = word_start + prev_frag_end;

                    if let Some((frag_end, _)) = char_iter.next() {
                        last_passing_breakpoint =
                            self.measure(&cur_slice[..frag_end], params, false);

                        prev_frag_end = frag_end;
                    } else {
                        break;
                    }
                }

                return Some(prev_char_index);
            } else if measure.0 > remaining_width {
                //The word is wider than our remaining width, return the end of
                //the line.
                return Some(line_end);
            } else {
                //Space remains for our current word, move up the word pointer.
                line_end = word_end;
                is_start_of_line = is_start_of_line && text[0..line_end].trim().is_empty();

                //If the additional space were to cause an overflow, then
                //return now.
                remaining_width -= measure.0;
                if remaining_width < Twips::from_pixels(0.0) {
                    return Some(word_end);
                }
            }
        }

        None
    }

    pub fn descriptor(&self) -> &FontDescriptor {
        &self.0.descriptor
    }
}

#[derive(Debug, Clone)]
pub struct Glyph {
    // Handle to registered shape.
    // If None, it'll be loaded lazily on first render of this glyph.
    shape_handle: RefCell<Option<ShapeHandle>>,

    // Same shape as one in swf_glyph, but wrapped in an swf::Shape;
    // For use in hit tests. Created lazily on first use.
    // (todo: refactor hit tests to not require this?
    // this literally copies the shape_record, which is wasteful...)
    shape: RefCell<Option<swf::Shape>>,

    // The underlying glyph record, containing its shape.
    swf_glyph: swf::Glyph,
}

impl Glyph {
    pub fn as_shape(&self) -> Ref<'_, swf::Shape> {
        self.shape
            .borrow_mut()
            .get_or_insert_with(|| ruffle_render::shape_utils::swf_glyph_to_shape(&self.swf_glyph));
        Ref::map(self.shape.borrow(), |s| s.as_ref().unwrap())
    }

    pub fn shape_handle(&self, renderer: &mut dyn RenderBackend) -> ShapeHandle {
        self.shape_handle
            .borrow_mut()
            .get_or_insert_with(|| {
                renderer.register_shape((&*self.as_shape()).into(), &NullBitmapSource)
            })
            .clone()
    }
}

/// Structure which identifies a particular font by name and properties.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Collect)]
#[collect(require_static)]
pub struct FontDescriptor {
    name: String,
    is_bold: bool,
    is_italic: bool,
}

impl FontDescriptor {
    /// Obtain a font descriptor from a SWF font tag.
    pub fn from_swf_tag(val: &swf::Font, encoding: &'static swf::Encoding) -> Self {
        let name = val.name.to_string_lossy(encoding);

        Self {
            name,
            is_bold: val.flags.contains(swf::FontFlag::IS_BOLD),
            is_italic: val.flags.contains(swf::FontFlag::IS_ITALIC),
        }
    }

    /// Obtain a font descriptor from a name/bold/italic triplet.
    pub fn from_parts(name: &str, is_bold: bool, is_italic: bool) -> Self {
        let mut name = name.to_string();

        if let Some(first_null) = name.find('\0') {
            name.truncate(first_null);
        };

        Self {
            name,
            is_bold,
            is_italic,
        }
    }

    /// Get the name of the font class this descriptor references.
    pub fn class(&self) -> &str {
        &self.name
    }

    /// Get the boldness of the described font.
    pub fn bold(&self) -> bool {
        self.is_bold
    }

    /// Get the italic-ness of the described font.
    pub fn italic(&self) -> bool {
        self.is_italic
    }
}

/// The text rendering engine that a text field should use.
/// This is controlled by the "Anti-alias" setting in the Flash IDE.
/// Using "Anti-alias for readibility" switches to the "Advanced" text
/// rendering engine.
#[derive(Debug, PartialEq, Clone)]
pub enum TextRenderSettings {
    /// This text should render with the standard rendering engine.
    /// Set via "Anti-alias for animation" in the Flash IDE.
    ///
    /// The `grid_fit`, `thickness`, and `sharpness` parameters are present
    /// because they are retained when switching from `Advanced` to `Normal`
    /// rendering and vice versa. They are not used in Normal rendering.
    Normal {
        grid_fit: TextGridFit,
        thickness: f32,
        sharpness: f32,
    },

    /// This text should render with the advanced rendering engine.
    /// Set via "Anti-alias for readibility" in the Flash IDE.
    /// The parameters are set via the CSMTextSettings SWF tag.
    /// Ruffle does not support this currently, but this also affects
    /// hit-testing behavior.
    Advanced {
        grid_fit: TextGridFit,
        thickness: f32,
        sharpness: f32,
    },
}

impl TextRenderSettings {
    pub fn is_advanced(&self) -> bool {
        matches!(self, TextRenderSettings::Advanced { .. })
    }

    pub fn with_advanced_rendering(self) -> Self {
        match self {
            TextRenderSettings::Advanced { .. } => self,
            TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness,
            } => TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness,
            },
        }
    }

    pub fn with_normal_rendering(self) -> Self {
        match self {
            TextRenderSettings::Normal { .. } => self,
            TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness,
            } => TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness,
            },
        }
    }

    pub fn sharpness(&self) -> f32 {
        match self {
            TextRenderSettings::Normal { sharpness, .. } => *sharpness,
            TextRenderSettings::Advanced { sharpness, .. } => *sharpness,
        }
    }

    pub fn with_sharpness(self, sharpness: f32) -> Self {
        match self {
            TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness: _,
            } => TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness,
            },
            TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness: _,
            } => TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness,
            },
        }
    }

    pub fn thickness(&self) -> f32 {
        match self {
            TextRenderSettings::Normal { thickness, .. } => *thickness,
            TextRenderSettings::Advanced { thickness, .. } => *thickness,
        }
    }

    pub fn with_thickness(self, thickness: f32) -> Self {
        match self {
            TextRenderSettings::Normal {
                grid_fit,
                thickness: _,
                sharpness,
            } => TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness,
            },
            TextRenderSettings::Advanced {
                grid_fit,
                thickness: _,
                sharpness,
            } => TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness,
            },
        }
    }

    pub fn grid_fit(&self) -> swf::TextGridFit {
        match self {
            TextRenderSettings::Normal { grid_fit, .. } => *grid_fit,
            TextRenderSettings::Advanced { grid_fit, .. } => *grid_fit,
        }
    }

    pub fn with_grid_fit(self, grid_fit: TextGridFit) -> Self {
        match self {
            TextRenderSettings::Normal {
                grid_fit: _,
                thickness,
                sharpness,
            } => TextRenderSettings::Normal {
                grid_fit,
                thickness,
                sharpness,
            },
            TextRenderSettings::Advanced {
                grid_fit: _,
                thickness,
                sharpness,
            } => TextRenderSettings::Advanced {
                grid_fit,
                thickness,
                sharpness,
            },
        }
    }
}

impl From<swf::CsmTextSettings> for TextRenderSettings {
    fn from(settings: swf::CsmTextSettings) -> Self {
        if settings.use_advanced_rendering {
            TextRenderSettings::Advanced {
                grid_fit: settings.grid_fit,
                thickness: settings.thickness,
                sharpness: settings.sharpness,
            }
        } else {
            TextRenderSettings::default()
        }
    }
}

impl Default for TextRenderSettings {
    fn default() -> Self {
        Self::Normal {
            grid_fit: TextGridFit::Pixel,
            thickness: 0.0,
            sharpness: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::font::{EvalParameters, Font};
    use crate::player::Player;
    use crate::string::WStr;
    use gc_arena::{rootless_arena, Mutation};
    use ruffle_render::backend::{null::NullRenderer, ViewportDimensions};
    use swf::Twips;

    fn with_device_font<F>(callback: F)
    where
        F: for<'gc> FnOnce(&Mutation<'gc>, Font<'gc>),
    {
        rootless_arena(|mc| {
            let mut renderer = NullRenderer::new(ViewportDimensions {
                width: 0,
                height: 0,
                scale_factor: 1.0,
            });
            let device_font = Player::load_device_font(mc, &mut renderer);

            callback(mc, device_font);
        })
    }

    #[test]
    fn wrap_line_no_breakpoint() {
        with_device_font(|_mc, df| {
            let params =
                EvalParameters::from_parts(Twips::from_pixels(12.0), Twips::from_pixels(0.0), true);
            let string = WStr::from_units(b"abcdefghijklmnopqrstuv");
            let breakpoint = df.wrap_line(
                string,
                params,
                Twips::from_pixels(200.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(None, breakpoint);
        });
    }

    #[test]
    fn wrap_line_breakpoint_every_word() {
        with_device_font(|_mc, df| {
            let params =
                EvalParameters::from_parts(Twips::from_pixels(12.0), Twips::from_pixels(0.0), true);
            let string = WStr::from_units(b"abcd efgh ijkl mnop");
            let mut last_bp = 0;
            let breakpoint = df.wrap_line(
                string,
                params,
                Twips::from_pixels(35.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(4), breakpoint);

            last_bp += breakpoint.unwrap() + 1;

            let breakpoint2 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(35.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(4), breakpoint2);

            last_bp += breakpoint2.unwrap() + 1;

            let breakpoint3 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(35.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(4), breakpoint3);

            last_bp += breakpoint3.unwrap() + 1;

            let breakpoint4 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(35.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(None, breakpoint4);
        });
    }

    #[test]
    fn wrap_line_breakpoint_no_room() {
        with_device_font(|_mc, df| {
            let params =
                EvalParameters::from_parts(Twips::from_pixels(12.0), Twips::from_pixels(0.0), true);
            let string = WStr::from_units(b"abcd efgh ijkl mnop");
            let breakpoint = df.wrap_line(
                string,
                params,
                Twips::from_pixels(30.0),
                Twips::from_pixels(29.0),
                false,
            );

            assert_eq!(Some(0), breakpoint);
        });
    }

    #[test]
    fn wrap_line_breakpoint_irregular_sized_words() {
        with_device_font(|_mc, df| {
            let params =
                EvalParameters::from_parts(Twips::from_pixels(12.0), Twips::from_pixels(0.0), true);
            let string = WStr::from_units(b"abcdi j kl mnop q rstuv");
            let mut last_bp = 0;
            let breakpoint = df.wrap_line(
                string,
                params,
                Twips::from_pixels(37.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(5), breakpoint);

            last_bp += breakpoint.unwrap() + 1;

            let breakpoint2 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(37.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(4), breakpoint2);

            last_bp += breakpoint2.unwrap() + 1;

            let breakpoint3 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(37.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(4), breakpoint3);

            last_bp += breakpoint3.unwrap() + 1;

            let breakpoint4 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(37.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(Some(1), breakpoint4);

            last_bp += breakpoint4.unwrap() + 1;

            let breakpoint5 = df.wrap_line(
                &string[last_bp..],
                params,
                Twips::from_pixels(37.0),
                Twips::from_pixels(0.0),
                true,
            );

            assert_eq!(None, breakpoint5);
        });
    }
}
