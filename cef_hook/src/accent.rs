// Windows accent color → --SystemAccentColor* CSS variables.
//
// Port of Millennium's src/bindings/sys_accent_col.cc + frontend
// SystemColors.ts / browser-init.ts appendAccentColor: reads the user's
// immersive accent palette from uxtheme.dll (undocumented Win10/11 APIs) and
// emits the :root block themes like Minimal-Dark-for-Steam depend on
// (--SystemAccentColorAccent etc.). Falls back to a Steam-blue palette when
// the APIs are unavailable.

use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type GetImmersiveUserColorSetPreferenceFn = unsafe extern "system" fn(i32, i32) -> u32;
type GetImmersiveColorFromColorSetExFn = unsafe extern "system" fn(u32, u32, i32, u32) -> u32;
type GetImmersiveColorTypeFromNameFn = unsafe extern "system" fn(*const u16) -> i32;

const ACCENT_NAMES: [&str; 7] = [
    "ImmersiveSystemAccent",
    "ImmersiveSystemAccentLight1",
    "ImmersiveSystemAccentLight2",
    "ImmersiveSystemAccentLight3",
    "ImmersiveSystemAccentDark1",
    "ImmersiveSystemAccentDark2",
    "ImmersiveSystemAccentDark3",
];

// Fallback palette (Steam-ish blue with light/dark variants) used when the
// uxtheme APIs fail: [accent, light1..3, dark1..3].
const FALLBACK: [u32; 7] = [
    0xFFC066, // accent (#66c0ff, BGR)
    0xFFD18F, 0xFFDFAB, 0xFFECCC, 0xE8A64D, 0xC98B3B, 0xAA702A,
];

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// DWORD → "#rrggbb" (color is 0xBBGGRR / AABBGGRR).
fn to_hex(color: u32) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        color & 0xFF,
        (color >> 8) & 0xFF,
        (color >> 16) & 0xFF
    )
}

fn to_rgb(color: u32) -> String {
    format!(
        "{}, {}, {}",
        color & 0xFF,
        (color >> 8) & 0xFF,
        (color >> 16) & 0xFF
    )
}

/// Mix toward white (pct > 0) or black (pct < 0), like Millennium's
/// adjust_color_intensity fallback extrapolation.
fn adjust(color: u32, pct: i32) -> u32 {
    let ch = |c: u32| -> u32 {
        let v = (c & 0xFF) as i32;
        let out = if pct >= 0 {
            v + (255 - v) * pct / 100
        } else {
            v + v * pct / 100
        };
        out.clamp(0, 255) as u32
    };
    let r = ch(color);
    let g = ch(color >> 8);
    let b = ch(color >> 16);
    (b << 16) | (g << 8) | r
}

fn read_accent_colors() -> [u32; 7] {
    unsafe {
        let uxtheme = LoadLibraryW(wide_null("uxtheme.dll").as_ptr());
        if uxtheme.is_null() {
            return derive_fallback(0);
        }

        let get_set: Option<GetImmersiveUserColorSetPreferenceFn> = std::mem::transmute(
            GetProcAddress(uxtheme, b"GetImmersiveUserColorSetPreference\0".as_ptr()),
        );
        let get_color: Option<GetImmersiveColorFromColorSetExFn> = std::mem::transmute(
            GetProcAddress(uxtheme, b"GetImmersiveColorFromColorSetEx\0".as_ptr()),
        );
        // GetImmersiveColorTypeFromName is exported without a name —
        // consistently ordinal 96 on Windows 10/11 (same as Millennium).
        let get_type: Option<GetImmersiveColorTypeFromNameFn> =
            std::mem::transmute(GetProcAddress(uxtheme, 96usize as *const u8));

        let (get_set, get_color, get_type) = match (get_set, get_color, get_type) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                windows_sys::Win32::Foundation::FreeLibrary(uxtheme);
                return derive_fallback(0);
            }
        };

        let color_set = get_set(0, 0);
        let mut colors = [0u32; 7];
        for (i, name) in ACCENT_NAMES.iter().enumerate() {
            let wide = wide_null(name);
            let color_type = get_type(wide.as_ptr());
            if color_type != -1 {
                colors[i] = get_color(color_set, color_type as u32, 0, 0);
            }
        }

        windows_sys::Win32::Foundation::FreeLibrary(uxtheme);

        if colors[0] == 0 {
            derive_fallback(0)
        } else {
            colors
        }
    }
}

fn derive_fallback(accent: u32) -> [u32; 7] {
    if accent != 0 {
        [
            accent,
            adjust(accent, 15),
            adjust(accent, 30),
            adjust(accent, 45),
            adjust(accent, -15),
            adjust(accent, -30),
            adjust(accent, -45),
        ]
    } else {
        FALLBACK
    }
}

/// The `:root` CSS block with every --SystemAccentColor* variable, in both
/// naming styles Millennium emits (`--SystemAccentColor` from SystemColors.ts
/// and `--SystemAccentColorAccent` from appendAccentColor in browser-init.ts).
/// Cached for the lifetime of the process.
pub fn system_accent_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        let c = read_accent_colors();
        let mut css = String::from(":root {\n");
        let push = |css: &mut String, name: &str, color: u32| {
            if name.is_empty() {
                css.push_str(&format!("    --SystemAccentColor: {};\n", to_hex(color)));
                css.push_str(&format!(
                    "    --SystemAccentColor-RGB: {};\n",
                    to_rgb(color)
                ));
            } else {
                css.push_str(&format!(
                    "    --SystemAccentColor{}: {};\n",
                    name,
                    to_hex(color)
                ));
                css.push_str(&format!(
                    "    --SystemAccentColor{}-RGB: {};\n",
                    name,
                    to_rgb(color)
                ));
            }
        };
        push(&mut css, "", c[0]);
        push(&mut css, "Accent", c[0]);
        push(&mut css, "Light1", c[1]);
        push(&mut css, "Light2", c[2]);
        push(&mut css, "Light3", c[3]);
        push(&mut css, "Dark1", c[4]);
        push(&mut css, "Dark2", c[5]);
        push(&mut css, "Dark3", c[6]);
        push(&mut css, "OriginalAccent", c[0]);
        css.push_str("}\n");
        css
    })
}
