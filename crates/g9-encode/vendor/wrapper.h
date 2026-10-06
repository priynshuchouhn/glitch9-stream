/* bindgen entry point: pulls in the vendored NVENC API header.
 * On Windows, nvEncodeAPI.h includes <windows.h> for GUID/HANDLE, resolved via
 * the Windows SDK include paths that libclang discovers from the MSVC environment. */
#include "nvEncodeAPI.h"
