/// A rectangle on the X screen, in root window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Clip the region to a `screen_width` x `screen_height` screen and round the size down to
    /// even dimensions (required by yuv420p encoding).
    pub fn fit_for_encoding(self, screen_width: u32, screen_height: u32) -> Option<Self> {
        let x = self.x.clamp(0, screen_width as i32);
        let y = self.y.clamp(0, screen_height as i32);
        let right = (self.x + self.width as i32).min(screen_width as i32);
        let bottom = (self.y + self.height as i32).min(screen_height as i32);
        let width = (right - x).max(0) as u32 & !1;
        let height = (bottom - y).max(0) as u32 & !1;
        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::Region;

    #[test]
    fn rounds_down_to_even_size() {
        let region = Region::new(0, 0, 1921, 1081).fit_for_encoding(4000, 4000);
        assert_eq!(region, Some(Region::new(0, 0, 1920, 1080)));
    }

    #[test]
    fn clips_to_screen() {
        let region = Region::new(-10, 100, 500, 500).fit_for_encoding(400, 400);
        assert_eq!(region, Some(Region::new(0, 100, 400, 300)));
    }

    #[test]
    fn rejects_offscreen_region() {
        assert_eq!(
            Region::new(500, 500, 100, 100).fit_for_encoding(400, 400),
            None
        );
    }
}
