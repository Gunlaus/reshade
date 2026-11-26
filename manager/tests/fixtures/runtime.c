#include <windows.h>

__declspec(dllexport) int manager_fixture(void)
{
#ifdef FULL_ADDON
    return 2;
#else
    return 1;
#endif
}
