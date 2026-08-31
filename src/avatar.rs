//! A pixel avatar, unique to whoever is signed in, small enough to ride in the
//! toolbar alongside the brand.
//!
//! The seed is a domain-separated SHA-256 of the refresh token rather than the
//! token itself. That gives an account the same face on every machine it signs
//! in from, while what reaches the screen is a dozen bits of a 256-bit digest
//! and a colour drawn from one more byte of it — an
//! identifier, with no way back to the credential it was derived from.
//!
//! Nothing here ever renders or stores the token, and the digest is taken over
//! a constant prefix so the same secret used elsewhere could not produce a
//! matching value.

use ratatui::style::Color;
use sha2::{Digest, Sha256};

use crate::theme;

/// The grid, in pixels. The toolbar gives the avatar a single text row, and a
/// braille cell packs two columns by four rows of dots into one — so four is
/// every row there is to spend, and six columns keeps the face wider than it is
/// tall rather than a cramped square nobody can read.
pub const WIDTH: usize = 6;
pub const HEIGHT: usize = 4;
/// Only half the columns are drawn from the digest; the rest are mirrored.
const HALF: usize = WIDTH / 2;
/// Cells the badge occupies — two pixel columns to each.
pub const CELLS: u16 = (WIDTH / 2) as u16;

/// Braille dot bits, in the order the code points number them: down the left
/// column, then down the right. Laid out here rather than computed because the
/// eighth-dot extension broke the arithmetic progression of the original six.
const DOTS: [[u8; HEIGHT]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
/// Braille patterns start here; the dot bits are added to it.
const BRAILLE_BASE: u32 = 0x2800;

/// Keeps this digest distinct from any other use of the same token.
const DOMAIN: &[u8] = b"anacraft/avatar/v1";

/// The face shown when there is no account to derive one from: the demo, and a
/// dashboard that has not signed in yet. Fixed, so the site's captures don't
/// change every time they are regenerated.
const DEMO_SEED: &[u8] = b"anacraft/avatar/demo";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Avatar {
    /// Lit pixels, row-major, mirrored down the vertical centre.
    grid: [[bool; WIDTH]; HEIGHT],
    /// Index into the palette ramp, held rather than resolved so the avatar
    /// re-colours with the rest of the dashboard when the theme is cycled.
    hue: usize,
}

impl Avatar {
    /// Derive a face from arbitrary bytes.
    fn from_seed(seed: &[u8]) -> Avatar {
        let digest = Sha256::digest([DOMAIN, seed].concat());

        // Mirroring the grid is what separates an avatar from static: the eye
        // reads symmetry as a face, and an asymmetric 6x6 reads as dirt on the
        // screen. Only the left half comes from the digest.
        let mut grid = [[false; WIDTH]; HEIGHT];
        for (row, cells) in grid.iter_mut().enumerate() {
            for col in 0..HALF {
                let bit = row * HALF + col;
                let lit = digest[bit / 8] >> (bit % 8) & 1 == 1;
                cells[col] = lit;
                cells[WIDTH - 1 - col] = lit;
            }
        }

        Avatar {
            grid,
            hue: digest[31] as usize,
        }
    }

    /// The face for whoever is signed in, falling back to the demo's when
    /// there is no token to read. A failure to load one is not worth surfacing
    /// here — every path that actually needs the credential says so itself.
    pub fn for_account() -> Avatar {
        match crate::auth::Tokens::load() {
            Ok(Some(tokens)) => Avatar::from_seed(tokens.refresh_token.as_bytes()),
            _ => Avatar::demo(),
        }
    }

    pub fn demo() -> Avatar {
        Avatar::from_seed(DEMO_SEED)
    }

    /// The badge, as braille. One cell carries two columns of four dots, which
    /// is what fits four pixel rows inside a toolbar one row tall — the same
    /// trick the spinner already leans on, so it needs no glyph a terminal
    /// running this dashboard does not already draw.
    pub fn glyphs(&self) -> String {
        (0..WIDTH)
            .step_by(2)
            .map(|col| {
                let mut bits = 0u8;
                for (offset, dots) in DOTS.iter().enumerate() {
                    for (row, dot) in dots.iter().enumerate() {
                        if self.grid[row][col + offset] {
                            bits |= dot;
                        }
                    }
                }
                char::from_u32(BRAILLE_BASE + bits as u32).expect("braille block is contiguous")
            })
            .collect()
    }

    /// Held as a ramp index rather than a resolved colour, so the face
    /// re-colours with the rest of the dashboard when the theme is cycled.
    pub fn color(&self) -> Color {
        theme::ramp(self.hue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit_count(avatar: &Avatar) -> usize {
        avatar
            .grid
            .iter()
            .flat_map(|row| row.iter())
            .filter(|on| **on)
            .count()
    }

    #[test]
    fn the_same_account_always_gets_the_same_face() {
        assert_eq!(
            Avatar::from_seed(b"1//0aRefreshToken"),
            Avatar::from_seed(b"1//0aRefreshToken")
        );
    }

    #[test]
    fn different_accounts_get_different_faces() {
        let one = Avatar::from_seed(b"1//0aRefreshToken");
        let two = Avatar::from_seed(b"1//0bRefreshToken");
        assert_ne!(one, two, "a one-character change should redraw the face");
    }

    #[test]
    fn a_prefix_of_a_token_is_not_a_near_miss() {
        // The digest is what makes the face unlinkable to the credential: a
        // token and its own prefix must not produce neighbouring grids.
        let full = Avatar::from_seed(b"1//0aRefreshToken");
        let prefix = Avatar::from_seed(b"1//0aRefresh");
        assert_ne!(full, prefix);
    }

    #[test]
    fn the_face_is_mirrored_down_the_middle() {
        for seed in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            let avatar = Avatar::from_seed(seed);
            for (row, cells) in avatar.grid.iter().enumerate() {
                for (col, lit) in cells.iter().enumerate() {
                    assert_eq!(
                        *lit,
                        cells[WIDTH - 1 - col],
                        "column {col} of row {row} is not mirrored"
                    );
                }
            }
        }
    }

    #[test]
    fn the_demo_face_never_moves() {
        // The site's captures are regenerated from this; a face that drifted
        // would show up as a diff in every one of them.
        assert_eq!(Avatar::demo(), Avatar::demo());
        assert_eq!(Avatar::demo(), Avatar::from_seed(DEMO_SEED));
    }

    #[test]
    fn a_face_is_neither_blank_nor_solid() {
        // Both extremes are legal digests and both would read as a bug on
        // screen. Across a spread of seeds we should see neither.
        for n in 0..64u32 {
            let avatar = Avatar::from_seed(&n.to_le_bytes());
            let count = lit_count(&avatar);
            assert!(count > 0, "seed {n} drew an empty avatar");
            assert!(count < WIDTH * HEIGHT, "seed {n} drew a solid avatar");
        }
    }

    #[test]
    fn the_badge_is_exactly_the_width_the_toolbar_reserves() {
        // The header subtracts CELLS from the chip budget and pads the gap by
        // the same number. A badge of any other width would drag the whole bar
        // off its right edge.
        for n in 0..64u32 {
            let glyphs = Avatar::from_seed(&n.to_le_bytes()).glyphs();
            assert_eq!(glyphs.chars().count(), CELLS as usize);
        }
    }

    #[test]
    fn the_badge_is_braille_and_nothing_else() {
        // Anything outside the block would be a width the terminal draws
        // differently from what the layout budgeted for.
        for n in 0..64u32 {
            for ch in Avatar::from_seed(&n.to_le_bytes()).glyphs().chars() {
                let point = ch as u32;
                assert!(
                    (BRAILLE_BASE..BRAILLE_BASE + 256).contains(&point),
                    "{ch:?} is not a braille pattern"
                );
            }
        }
    }

    #[test]
    fn every_pixel_reaches_a_dot() {
        // Walks the mapping the other way: light one pixel at a time and check
        // exactly one dot in the badge answers for it.
        for col in 0..WIDTH {
            for (row, dot) in DOTS[col % 2].iter().enumerate() {
                let mut avatar = Avatar::from_seed(b"blank");
                avatar.grid = [[false; WIDTH]; HEIGHT];
                avatar.grid[row][col] = true;

                let glyphs: Vec<char> = avatar.glyphs().chars().collect();
                let bits: u32 = glyphs.iter().map(|ch| *ch as u32 - BRAILLE_BASE).sum();
                assert_eq!(bits.count_ones(), 1, "pixel ({row}, {col}) lit {bits} dots");
                assert_eq!(
                    glyphs[col / 2] as u32 - BRAILLE_BASE,
                    *dot as u32,
                    "pixel ({row}, {col}) landed on the wrong dot"
                );
            }
        }
    }

    #[test]
    fn an_empty_grid_draws_a_blank_cell() {
        let mut avatar = Avatar::from_seed(b"blank");
        avatar.grid = [[false; WIDTH]; HEIGHT];
        assert!(avatar.glyphs().chars().all(|ch| ch as u32 == BRAILLE_BASE));
    }
}
