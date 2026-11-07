// C ABI for the Consistent CEO mobile Rust core (mobile/rust).
//
// Built into CWMobile.xcframework (iOS, a static CWMobile.framework per
// platform; Swift imports it as `import CWMobile`) and libcw_mobile.so
// (Android). See mobile/rust/src/lib.rs for the wire contract.

#ifndef CW_MOBILE_H
#define CW_MOBILE_H

#include <stdint.h>

#if !defined(__clang__)
#define _Nullable
#endif

#ifdef __cplusplus
extern "C" {
#endif

/** Version string ("consistent-world-mobile 1.1.0"). Free with cw_free_string. */
char *_Nullable cw_version(void);

/**
 * Parse a free-form reminder into JSON.
 *
 * Input:  {"text": "remind me to call Mom tomorrow 9am", "utcOffsetMinutes": 780}
 *         (utcOffsetMinutes is optional; plain text without JSON also works)
 * Output: {"what":"call Mom","fireAt":"2026-11-05T20:00:00.000Z","inMinutes":null,"confidence":0.9}
 *
 * Returns NULL when there is no result (NULL or non-UTF-8 input, an internal
 * error). Free a non-NULL result with cw_free_string.
 */
char *_Nullable cw_parse_reminder(const char *_Nullable input);

/**
 * Parse a free-form note into JSON.
 *
 * Input:  {"text": "buy milk tomorrow"}
 * Output: {"content":"buy milk","when":"tomorrow"}
 *
 * Returns NULL when there is no result. Free a non-NULL result with
 * cw_free_string.
 */
char *_Nullable cw_parse_note(const char *_Nullable input);

/** Free a string returned by any cw_* function. NULL is a no-op. */
void cw_free_string(char *_Nullable s);

#ifdef __cplusplus
}
#endif

#endif // CW_MOBILE_H
