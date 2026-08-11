/*
 * wsock32.dll bootstrap for LumaForge CDP Proxy
 *
 * Masquerades as wsock32.dll (NOT on KnownDLLs) in the Steam directory.
 * Forwards all real wsock32 exports to ws2_32.dll / MSWSOCK.dll via .def file.
 * Spawns a thread to load lumaforge/lumaforge.dll after DllMain returns
 * (avoids loader lock deadlock from calling LoadLibraryW inside DllMain).
 */

typedef unsigned long DWORD;
typedef int BOOL;
typedef void *HANDLE;
typedef void *HMODULE;
typedef unsigned short WCHAR;
typedef DWORD (__stdcall *LPTHREAD_START_ROUTINE)(void*);

#define MAX_PATH 260
#define DLL_PROCESS_ATTACH 1

__declspec(dllimport) DWORD __stdcall GetModuleFileNameW(HANDLE hModule, WCHAR *lpFilename, DWORD nSize);
__declspec(dllimport) HMODULE __stdcall LoadLibraryW(const WCHAR *lpLibFileName);
__declspec(dllimport) HANDLE __stdcall CreateThread(void*, DWORD, LPTHREAD_START_ROUTINE, void*, DWORD, DWORD*);
__declspec(dllimport) void __stdcall Sleep(DWORD);

static BOOL is_steam_client(void) {
    WCHAR path[MAX_PATH];
    DWORD len = GetModuleFileNameW(0, path, MAX_PATH);
    if (len == 0 || len >= MAX_PATH) return 0;
    int i = (int)len - 1;
    while (i >= 0 && path[i] != L'\\' && path[i] != L'/') i--;
    WCHAR *exe = &path[i + 1];
    const WCHAR *target = L"steam.exe";
    for (int j = 0; ; j++) {
        WCHAR a = exe[j]; WCHAR b = target[j];
        if (a >= L'A' && a <= L'Z') a += 32;
        if (b >= L'A' && b <= L'Z') b += 32;
        if (a != b) return 0;
        if (a == 0) break;
    }
    return 1;
}

static DWORD __stdcall loader(void *param) {
    Sleep(200);
    LoadLibraryW((const WCHAR *)param);
    return 0;
}

BOOL __stdcall DllMain(HANDLE hinstDLL, DWORD fdwReason, void *lpvReserved) {
    (void)lpvReserved;
    if (fdwReason == DLL_PROCESS_ATTACH && is_steam_client()) {
        WCHAR self[MAX_PATH];
        DWORD len = GetModuleFileNameW(hinstDLL, self, MAX_PATH);
        if (len > 0 && len < MAX_PATH) {
            int i = (int)len - 1;
            while (i >= 0 && self[i] != L'\\') i--;
            self[i + 1] = L'\0';

            static WCHAR dll_path[MAX_PATH];
            int j = 0;
            while (self[j]) { dll_path[j] = self[j]; j++; }

            const WCHAR *rel = L"lumaforge\\lumaforge.dll";
            int k = 0;
            while (rel[k]) { dll_path[j++] = rel[k++]; }
            dll_path[j] = L'\0';

            CreateThread(0, 0, loader, dll_path, 0, 0);
        }
    }
    return 1;
}
