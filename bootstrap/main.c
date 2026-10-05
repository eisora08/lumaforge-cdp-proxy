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
#define GENERIC_WRITE_ 0x40000000
#define CREATE_ALWAYS_ 2
#define FILE_ATTRIBUTE_NORMAL_ 0x80
#define INVALID_HANDLE_VALUE_ ((HANDLE)(long long)-1)

__declspec(dllimport) DWORD __stdcall GetModuleFileNameW(HANDLE hModule, WCHAR *lpFilename, DWORD nSize);
__declspec(dllimport) HMODULE __stdcall LoadLibraryW(const WCHAR *lpLibFileName);
__declspec(dllimport) HANDLE __stdcall CreateThread(void*, DWORD, LPTHREAD_START_ROUTINE, void*, DWORD, DWORD*);
__declspec(dllimport) void __stdcall Sleep(DWORD);
__declspec(dllimport) DWORD __stdcall GetLastError(void);
__declspec(dllimport) HANDLE __stdcall CreateFileW(const WCHAR*, DWORD, DWORD, void*, DWORD, DWORD, HANDLE);
__declspec(dllimport) BOOL __stdcall WriteFile(HANDLE, const void*, DWORD, DWORD*, void*);
__declspec(dllimport) BOOL __stdcall CloseHandle(HANDLE);
__declspec(dllimport) BOOL __stdcall DeleteFileW(const WCHAR*);

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

// Build "<dir>\lumaforge\load_error.txt" from the dll path passed to loader().
static void build_error_path(const WCHAR *dll_path, WCHAR *out) {
    int i = 0;
    while (dll_path[i] && i < MAX_PATH - 20) { out[i] = dll_path[i]; i++; }
    out[i] = 0;
    int j = i;
    while (j > 0 && out[j - 1] != L'\\') j--;   // start of "lumaforge.dll"
    const WCHAR *name = L"load_error.txt";       // 14 chars + NUL, fits buffer
    int k = 0;
    while (name[k]) { out[j + k] = name[k]; k++; }
    out[j + k] = 0;
}

static int u32_to_dec(DWORD v, char *out) {
    char tmp[12];
    int n = 0;
    do { tmp[n++] = (char)('0' + (v % 10)); v /= 10; } while (v && n < 12);
    for (int i = 0; i < n; i++) out[i] = tmp[n - 1 - i];
    return n;
}

static DWORD __stdcall loader(void *param) {
    const WCHAR *dll_path = (const WCHAR *)param;
    Sleep(200);
    HMODULE h = LoadLibraryW(dll_path);

    WCHAR err_path[MAX_PATH];
    build_error_path(dll_path, err_path);

    if (h) {
        DeleteFileW(err_path);   // clear stale error from a previous failed run
        return 0;
    }

    DWORD err = GetLastError();
    HANDLE f = CreateFileW(err_path, GENERIC_WRITE_, 0, 0, CREATE_ALWAYS_,
                           FILE_ATTRIBUTE_NORMAL_, 0);
    if (f != INVALID_HANDLE_VALUE_) {
        char msg[96];
        const char *prefix = "lumaforge.dll load failed, GetLastError=";
        int p = 0;
        while (prefix[p]) { msg[p] = prefix[p]; p++; }
        p += u32_to_dec(err, &msg[p]);
        msg[p++] = '\r'; msg[p++] = '\n';
        DWORD written;
        WriteFile(f, msg, (DWORD)p, &written, 0);
        CloseHandle(f);
    }
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
