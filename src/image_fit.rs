//! Pixel rectangle shared by image rendering and inverse pointer mapping.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageFit {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

impl ImageFit {
    pub fn new(source_width: u32, source_height: u32, width: u32, height: u32) -> Option<Self> {
        if source_width == 0 || source_height == 0 || width == 0 || height == 0 {
            return None;
        }
        let (fit_width, fit_height) = if u64::from(width) * u64::from(source_height)
            <= u64::from(height) * u64::from(source_width)
        {
            (
                width,
                (u64::from(width) * u64::from(source_height) / u64::from(source_width)).max(1)
                    as u32,
            )
        } else {
            (
                (u64::from(height) * u64::from(source_width) / u64::from(source_height)).max(1)
                    as u32,
                height,
            )
        };
        Some(Self {
            left: (width - fit_width) / 2,
            top: (height - fit_height) / 2,
            width: fit_width,
            height: fit_height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_content_pixels_for_scaling_letterboxing_and_odd_bars() {
        assert_eq!(
            ImageFit::new(2560, 1440, 1920, 1080),
            Some(ImageFit {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            ImageFit::new(2560, 1440, 1920, 1200),
            Some(ImageFit {
                left: 0,
                top: 60,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            ImageFit::new(1080, 1920, 1920, 1080),
            Some(ImageFit {
                left: 656,
                top: 0,
                width: 607,
                height: 1080
            })
        );
        assert!(ImageFit::new(0, 1920, 1920, 1080).is_none());
    }
}
