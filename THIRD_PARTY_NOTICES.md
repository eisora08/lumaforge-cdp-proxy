THIRD-PARTY SOFTWARE NOTICES

LumaForge CDP Proxy includes third-party open-source software.

==================================================
Millennium
==================================================

Project:
https://github.com/SteamClientHomebrew/Millennium

Copyright (c) SteamClientHomebrew

License: MIT License

LumaForge CDP Proxy contains code derived from Millennium, an open-source
modding framework for the desktop Steam Client. The following parts are
ported from or modeled on Millennium's implementation:

  - The theme pipeline: skin.json parsing, default theme patches, condition
    and dropdown/slider seeding, and theme activation on import
    (src/theme.rs, cef_hook/src/lib.rs - ports of ThemeParser.ts,
    setup_conditionals, and the patcher).
  - System accent color extraction and the --SystemAccentColor /
    --st-accent-* variable naming (cef_hook/src/accent.rs - port of
    sys_accent_col.cc and SystemColors.ts).
  - Window matching semantics for patches, including the html/body class
    alias handling (src/injector.rs, cef_hook/src/lib.rs - port of
    patcher/index.ts).
  - Popup window patching via g_PopupManager, and the `window.Millennium`
    compatibility shims (findElement, RouterHook) that community themes
    expect at runtime.
  - The Millennium active.json theme-state format
    ({"themes": {"activeTheme": "..."}}), read and written in place so
    Millennium and LumaForge can share installed themes.
  - The Linux libXtst.so.6 proxy loading approach (hook/src/lib.rs).

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

==================================================
fflate
==================================================

Project:
https://github.com/101arrowz/fflate

Copyright (c) 2020 Arjun Barrett

License: MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.