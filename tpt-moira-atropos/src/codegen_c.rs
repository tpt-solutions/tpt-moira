//! Stage-0 backend: emit the MIR as C source.
//!
//! This is the bootstrap path described in the design doc — a macro-free Moira
//! subset is translated to C and then compiled to a native binary by an
//! external C compiler. It is the reference backend for Phase 1; a from-scratch
//! native SSA code generator slots in behind the same `lower` + emit boundary.

use crate::mir;

/// The C runtime for `str`/`String` methods. Each helper is named
/// `moira_str_<method>`; a method call on a string receiver lowers to a call of it.
/// `f64` `Display`, as Rust prints it: the shortest decimal that reads back to the same
/// value, in positional notation (never an exponent). `2.5` is `2.5`, `2.0` is `2`,
/// `1e21` is `1` followed by 21 zeros. Writes into `out`, which must hold 400 bytes.
const F64_RUNTIME: &str = r#"#include <math.h>
static int moira_f64_format(char* out, double x) {
    int n = 0;
    if (isnan(x)) { strcpy(out, "NaN"); return 3; }
    if (signbit(x)) out[n++] = '-';
    double a = fabs(x);
    if (isinf(a)) { strcpy(out + n, "inf"); return n + 3; }
    if (a == 0.0) { out[n++] = '0'; out[n] = 0; return n; }
    char e[64];
    int prec;
    for (prec = 1; prec <= 17; prec++) {
        snprintf(e, sizeof e, "%.*e", prec - 1, a);
        if (strtod(e, NULL) == a) break;
    }
    /* `e` is `d[.ddd]e[+-]XX`: the significant digits and the decimal exponent. */
    char digits[32];
    int nd = 0;
    const char* p = e;
    while (*p && *p != 'e') {
        if (*p >= '0' && *p <= '9') digits[nd++] = *p;
        p++;
    }
    int exp10 = atoi(p + 1);
    while (nd > 1 && digits[nd - 1] == '0') nd--;
    if (exp10 >= 0) {
        for (int i = 0; i <= exp10; i++) out[n++] = i < nd ? digits[i] : '0';
        if (nd > exp10 + 1) {
            out[n++] = '.';
            for (int i = exp10 + 1; i < nd; i++) out[n++] = digits[i];
        }
    } else {
        out[n++] = '0';
        out[n++] = '.';
        for (int i = 0; i < -exp10 - 1; i++) out[n++] = '0';
        for (int i = 0; i < nd; i++) out[n++] = digits[i];
    }
    out[n] = 0;
    return n;
}
"#;

const STR_RUNTIME: &str = r#"#include <ctype.h>
static char* moira_str_dup_n(const char* s, size_t n) { char* r = (char*)malloc(n + 1); memcpy(r, s, n); r[n] = 0; return r; }
char* moira_str_dup(char* s) { return moira_str_dup_n(s, strlen(s)); }
// `Path::join` / `PathBuf::push`-style joining: `a/b`, with no doubled separator.
char* moira_path_join(char* a, char* b) {
    size_t n = strlen(a), m = strlen(b);
    int sep = n > 0 && a[n - 1] != '/' && a[n - 1] != '\\';
    char* r = (char*)malloc(n + m + (sep ? 1 : 0) + 1);
    memcpy(r, a, n);
    if (sep) r[n++] = '/';
    memcpy(r + n, b, m + 1);
    return r;
}
char* moira_str_to_string(char* s) { return moira_str_dup(s); }
char* moira_f64_to_string(double x) { char b[400]; moira_f64_format(b, x); return moira_str_dup(b); }
char* moira_int_to_string(long long x) { char b[32]; sprintf(b, "%lld", x); return moira_str_dup(b); }
char* moira_str_to_owned(char* s) { return moira_str_dup(s); }
char* moira_str_trim_start(char* s) { while (*s && isspace((unsigned char)*s)) s++; return moira_str_dup(s); }
char* moira_str_trim_end(char* s) { size_t n = strlen(s); while (n > 0 && isspace((unsigned char)s[n - 1])) n--; return moira_str_dup_n(s, n); }
char* moira_str_trim(char* s) { return moira_str_trim_end(moira_str_trim_start(s)); }
long long moira_sat_add(long long a, long long b) { long long r; if (__builtin_add_overflow(a, b, &r)) return b > 0 ? 9223372036854775807LL : (-9223372036854775807LL - 1); return r; }
long long moira_sat_mul(long long a, long long b) { long long r; if (__builtin_mul_overflow(a, b, &r)) return ((a > 0) == (b > 0)) ? 9223372036854775807LL : (-9223372036854775807LL - 1); return r; }
long long moira_sat_sub(long long a, long long b) { long long r; if (__builtin_sub_overflow(a, b, &r)) return b < 0 ? 9223372036854775807LL : (-9223372036854775807LL - 1); return r; }
long long moira_sat_sub_unsigned(long long a, long long b) { return a > b ? a - b : 0; }
char* moira_str_trim_end_matches_char(char* s, long long c) { size_t n = strlen(s); while (n > 0 && (long long)(unsigned char)s[n - 1] == c) n--; return moira_str_dup_n(s, n); }
char* moira_str_trim_start_matches_char(char* s, long long c) { while (*s && (long long)(unsigned char)*s == c) s++; return moira_str_dup(s); }
char* moira_str_trim_end_matches_str(char* s, char* p) { size_t m = strlen(p), n = strlen(s); if (m == 0) return moira_str_dup(s); while (n >= m && strncmp(s + n - m, p, m) == 0) n -= m; return moira_str_dup_n(s, n); }
char* moira_str_trim_start_matches_str(char* s, char* p) { size_t m = strlen(p); if (m == 0) return moira_str_dup(s); while (strncmp(s, p, m) == 0) s += m; return moira_str_dup(s); }
long long moira_str_len(char* s) { return (long long)strlen(s); }
long long moira_str_is_empty(char* s) { return s[0] == 0; }
long long moira_str_starts_with(char* s, char* p) { return strncmp(s, p, strlen(p)) == 0; }
long long moira_str_ends_with(char* s, char* p) { size_t n = strlen(s), m = strlen(p); return m <= n && strcmp(s + n - m, p) == 0; }
long long moira_str_contains(char* s, char* p) { return strstr(s, p) != NULL; }
char* moira_str_to_ascii_lowercase(char* s) { char* r = moira_str_dup(s); for (char* q = r; *q; q++) *q = (char)tolower((unsigned char)*q); return r; }
long long moira_str_push_str(char** s, char* t) { size_t a = strlen(*s), b = strlen(t); *s = (char*)realloc(*s, a + b + 1); memcpy(*s + a, t, b + 1); return 0; }
long long moira_str_push_char(char** s, long long c) { size_t a = strlen(*s); *s = (char*)realloc(*s, a + 2); (*s)[a] = (char)c; (*s)[a + 1] = 0; return 0; }
char* moira_str_after(char* s, long long n) { return moira_str_dup(s + n); }
char* moira_bool_str(long long b) { return moira_str_dup(b ? "true" : "false"); }
long long moira_str_eq(char* a, char* b) { return strcmp(a, b) == 0; }
// A `str.chars()` iterator: the string and the index of the next character.
typedef struct moira_chars { char* s; long long pos; } moira_chars;
long long moira_str_cmp(char* a, char* b) { int r = strcmp(a, b); return r < 0 ? -1 : (r > 0 ? 1 : 0); }
char* moira_char_str(long long c) { char* r = (char*)malloc(2); r[0] = (char)c; r[1] = 0; return r; }
long long moira_str_find(char* s, char* p) { char* m = strstr(s, p); return m ? (long long)(m - s) : -1; }
long long moira_char_is_ascii_alphanumeric(long long c) { return isalnum((int)c) != 0; }
long long moira_char_is_ascii_digit(long long c) { return c >= '0' && c <= '9'; }
long long moira_char_is_ascii_alphabetic(long long c) { return isalpha((int)c) != 0; }
long long moira_char_is_ascii_whitespace(long long c) { return isspace((int)c) != 0; }
long long moira_char_is_ascii_uppercase(long long c) { return c >= 'A' && c <= 'Z'; }
long long moira_char_is_ascii_lowercase(long long c) { return c >= 'a' && c <= 'z'; }
long long moira_char_is_alphanumeric(long long c) { return isalnum((int)c) != 0; }
long long moira_char_is_numeric(long long c) { return isdigit((int)c) != 0; }
long long moira_char_to_ascii_lowercase(long long c) { return tolower((int)c); }
long long moira_char_to_ascii_uppercase(long long c) { return toupper((int)c); }
long long moira_char_is_ascii_hexdigit(long long c) { return isxdigit((int)c) != 0; }
long long moira_char_to_digit(long long c, long long radix) { long long v = (c >= '0' && c <= '9') ? c - '0' : (c >= 'a' && c <= 'z') ? c - 'a' + 10 : (c >= 'A' && c <= 'Z') ? c - 'A' + 10 : -1; return (v >= 0 && v < radix) ? v : -1; }
long long moira_char_from_u32_valid(long long c) { return (c >= 0 && c <= 0x10FFFF && !(c >= 0xD800 && c <= 0xDFFF)); }
char* moira_str_before(char* s, long long n) { return moira_str_dup_n(s, (size_t)n); }
/* The bytes [a, b) of a string, in a new string. */
char* moira_str_range(char* s, long long a, long long b) { return moira_str_dup_n(s + a, (size_t)(b - a)); }
long long moira_line_next(char* s, long long i) { char* q = strchr(s + i, '\n'); return q ? (long long)(q - s) + 1 : (long long)strlen(s); }
#include <errno.h>
#include <ctype.h>
#include <time.h>
/* `parse::<T>()` as a status: 1 when the whole text is a number of the type (stored in *out), 0 otherwise. */
long long moira_try_parse_s(char* s, void* out, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s)) return 0; long long v = strtoll(s, &e, 10); long long lim = bits < 64 ? (1LL << (bits - 1)) : 0; if (*e == 0 && errno == 0 && (bits == 64 || (v >= -lim && v < lim))) { if (bits == 8) *(int8_t *)out = (int8_t)v; else if (bits == 16) *(int16_t *)out = (int16_t)v; else if (bits == 32) *(int32_t *)out = (int32_t)v; else *(long long*)out = v; return 1; } return 0; }
long long moira_try_parse_u(char* s, void* out, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s) || *s == '-') return 0; unsigned long long v = strtoull(s, &e, 10); unsigned long long mx = bits < 64 ? ((1ULL << bits) - 1) : 0ULL; if (*e == 0 && errno == 0 && (bits == 64 || v <= mx)) { if (bits == 8) *(uint8_t *)out = (uint8_t)v; else if (bits == 16) *(uint16_t *)out = (uint16_t)v; else if (bits == 32) *(uint32_t *)out = (uint32_t)v; else *(unsigned long long*)out = v; return 1; } return 0; }
long long moira_try_parse_f(char* s, double* out) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s)) return 0; double v = strtod(s, &e); if (*e == 0 && errno == 0) { *out = v; return 1; } return 0; }
static long long moira_f64_is_nan(double x) { return x != x; }
static long long moira_f64_is_infinite(double x) { return x == 1.0 / 0.0 || x == -1.0 / 0.0; }
static long long moira_f64_is_finite(double x) { return x - x == 0.0; }
static unsigned long long moira_f64_to_bits(double x) { unsigned long long u; memcpy(&u, &x, sizeof u); return u; }
static double moira_f64_from_bits(unsigned long long u) { double x; memcpy(&x, &u, sizeof x); return x; }
/* std: clocks, the environment, radix parsing. */
double moira_instant_now(void) { struct timespec ts; timespec_get(&ts, TIME_UTC); return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9; }
double moira_instant_elapsed(double t) { return moira_instant_now() - t; }
char* moira_env_get(char* k) { return getenv(k); }
char* moira_temp_dir(void) { char* t = getenv("TMPDIR"); if (!t) t = getenv("TEMP"); if (!t) t = getenv("TMP"); return moira_str_dup(t ? t : "."); }
long long moira_try_radix_u(char* s, long long radix, void* out, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s) || *s == '-') return 0; unsigned long long v = strtoull(s, &e, (int)radix); unsigned long long mx = bits < 64 ? ((1ULL << bits) - 1) : 0ULL; if (*e == 0 && errno == 0 && (bits == 64 || v <= mx)) { if (bits == 32) *(uint32_t *)out = (uint32_t)v; else *(unsigned long long*)out = v; return 1; } return 0; }
long long moira_try_radix_s(char* s, long long radix, void* out, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s)) return 0; long long v = strtoll(s, &e, (int)radix); long long lim = bits < 64 ? (1LL << (bits - 1)) : 0; if (*e == 0 && errno == 0 && (bits == 64 || (v >= -lim && v < lim))) { if (bits == 32) *(int32_t *)out = (int32_t)v; else *(long long*)out = v; return 1; } return 0; }
/* The process arguments (`env::args()`): main stores them on entry. */
int moira_argc = 0;
char** moira_argv = NULL;
long long moira_args_count(void) { return (long long)moira_argc; }
char** moira_args_data(void) { return moira_argv; }
/* std handles as values: files, directories, stdin, commands and the hasher. */
#ifdef _WIN32
#include <direct.h>
#define mkdir_ok(p) _mkdir(p)
#else
#include <sys/stat.h>
#define mkdir_ok(p) mkdir(p, 0777)
#endif
static void mkdir_or_ok(char* p) { mkdir_ok(p); }
long long moira_mkdir_all(char* p) { char* q = moira_str_dup(p); for (char* s = q + 1; *s; s++) { if (*s == '/' || *s == '\\') { char c = *s; *s = 0; mkdir_or_ok(q); *s = c; } } mkdir_or_ok(q); free(q); return 1; }
uint8_t* moira_fs_read_bytes(char* p, long long* len) { FILE* f = fopen(p, "rb"); *len = 0; if (!f) return NULL; fseek(f, 0, SEEK_END); long sz = ftell(f); fseek(f, 0, SEEK_SET); uint8_t* b = (uint8_t*)malloc((size_t)(sz > 0 ? sz : 1)); size_t n = fread(b, 1, (size_t)(sz > 0 ? sz : 0), f); fclose(f); *len = (long long)n; return b; }
char* moira_file_create(char* p) { FILE* f = fopen(p, "wb"); if (!f) return NULL; fclose(f); return moira_str_dup(p); }
char* moira_file_open(char* p) { FILE* f = fopen(p, "rb"); if (!f) return NULL; fclose(f); return moira_str_dup(p); }
long long moira_file_append(char* p, void* data, long long n) { FILE* f = fopen(p, "ab"); if (!f) return 0; size_t w = fwrite(data, 1, (size_t)n, f); fclose(f); return w == (size_t)n; }
long long moira_stdin_read_all(char** out) { size_t cap = 4096, len = 0; char* b = (char*)malloc(cap); size_t n; while ((n = fread(b + len, 1, cap - len, stdin)) > 0) { len += n; if (len == cap) { cap *= 2; b = (char*)realloc(b, cap); } } b[len] = 0; size_t a = strlen(*out); char* r = (char*)malloc(a + len + 1); memcpy(r, *out, a); memcpy(r + a, b, len + 1); *out = r; free(b); return 1; }
char* moira_cmd_arg(char* cmd, char* a) { char* r = (char*)malloc(strlen(cmd) + strlen(a) + 4); sprintf(r, "%s \"%s\"", cmd, a); return r; }
char* moira_cmd_output(char* cmd) { FILE* f = popen(cmd, "r"); if (!f) return NULL; size_t cap = 4096, len = 0; char* b = (char*)malloc(cap); size_t n; while ((n = fread(b + len, 1, cap - len, f)) > 0) { len += n; if (len == cap) { cap *= 2; b = (char*)realloc(b, cap); } } b[len] = 0; pclose(f); return b; }
long long moira_cmd_status(char* cmd) { return (long long)system(cmd); }
long long moira_hash_mix_u(unsigned long long* h, long long v) { *h = (*h ^ (unsigned long long)v) * 1099511628211ULL; return 0; }
long long moira_hash_mix_s(unsigned long long* h, char* s) { for (; *s; s++) *h = (*h ^ (unsigned char)*s) * 1099511628211ULL; *h = (*h ^ 0xff) * 1099511628211ULL; return 0; }
long long moira_str_clear(char** s) { *s = (char*)malloc(1); (*s)[0] = 0; return 0; }
long long moira_parse_s(char* s, long long d, int has_default, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s)) goto bad; long long v = strtoll(s, &e, 10); long long lim = bits < 64 ? (1LL << (bits - 1)) : 0; if (*e == 0 && errno == 0 && (bits == 64 || (v >= -lim && v < lim))) return v; bad: if (has_default) return d; __moira_panic(); return 0; }
unsigned long long moira_parse_u(char* s, unsigned long long d, int has_default, int bits) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s) || *s == '-') goto bad; unsigned long long v = strtoull(s, &e, 10); unsigned long long mx = bits < 64 ? ((1ULL << bits) - 1) : 0ULL; if (*e == 0 && errno == 0 && (bits == 64 || v <= mx)) return v; bad: if (has_default) return d; __moira_panic(); return 0; }
double moira_parse_f(char* s, double d, int has_default) { char* e; errno = 0; if (*s == 0 || isspace((unsigned char)*s)) goto bad; double v = strtod(s, &e); if (*e == 0 && errno == 0) return v; bad: if (has_default) return d; __moira_panic(); return 0; }
#include <dirent.h>
long long moira_path_exists(char* s) { FILE* f = fopen(s, "r"); if (f) fclose(f); DIR* d = opendir(s); if (d) closedir(d); return f != NULL || d != NULL; }
long long moira_path_is_file(char* s) { FILE* f = fopen(s, "r"); if (!f) return 0; fclose(f); DIR* d = opendir(s); if (d) { closedir(d); return 0; } return 1; }
/* `Path::file_stem`: the file name without its last extension, in a new string; NULL when the path has no file name. */
char* moira_path_file_stem(char* s) { size_t n = strlen(s); while (n > 0 && (s[n-1] == '/' || s[n-1] == '\\')) n--; size_t st = n; while (st > 0 && s[st-1] != '/' && s[st-1] != '\\') st--; size_t len = n - st; if (len == 0) return NULL; size_t dot = len; for (size_t k = len - 1; k > 0; k--) { if (st + k < n && s[st + k] == '.') { dot = k; break; } } char* r = (char*)malloc(dot + 1); memcpy(r, s + st, dot); r[dot] = 0; return r; }
long long moira_path_parent_index(char* s) { long long n = (long long)strlen(s); if (n == 1 && (s[0] == '/' || s[0] == '\\')) return -1; for (long long i = n - 1; i >= 0; i--) if (s[i] == '/' || s[i] == '\\') return i; return 0; }
long long moira_split_find(char* s, long long from, char* pat) { long long n = (long long)strlen(s), m = (long long)strlen(pat); for (long long i = from; i + m <= n; i++) if (memcmp(s + i, pat, (size_t)m) == 0) return i; return -1; }
long long moira_split_end(char* s, long long from, char* pat) { long long i = moira_split_find(s, from, pat); return i < 0 ? (long long)strlen(s) : i; }
long long moira_split_next(char* s, long long from, char* pat) { long long i = moira_split_find(s, from, pat); return i < 0 ? (long long)strlen(s) + 1 : i + (long long)strlen(pat); }
char* moira_line_text(char* s, long long i, long long next) { long long n = next - i; if (n > 0 && s[i + n - 1] == '\n') n--; if (n > 0 && s[i + n - 1] == '\r') n--; return moira_str_dup_n(s + i, (size_t)n); }
long long moira_fs_write(char* path, char* data) { FILE* f = fopen(path, "wb"); if (!f) return 0; size_t n = strlen(data); size_t w = fwrite(data, 1, n, f); fclose(f); return w == n; }
char* moira_fs_read_to_string(char* path) { FILE* f = fopen(path, "rb"); if (!f) return NULL; fseek(f, 0, SEEK_END); long sz = ftell(f); fseek(f, 0, SEEK_SET); char* b = (char*)malloc((size_t)sz + 1); size_t r = fread(b, 1, (size_t)sz, f); b[r] = 0; fclose(f); return b; }
long long moira_str_starts_with_char(char* s, long long c) { return s[0] != 0 && s[0] == (char)c; }
long long moira_str_ends_with_char(char* s, long long c) { size_t n = strlen(s); return n > 0 && s[n - 1] == (char)c; }
long long moira_str_contains_char(char* s, long long c) { return strchr(s, (int)c) != NULL; }
char* moira_str_replacen(char* s, char* from, char* to, long long limit) {
    size_t fl = strlen(from), tl = strlen(to);
    if (fl == 0) return moira_str_dup(s);
    size_t cap = strlen(s) + 1, n = 0;
    char* out = (char*)malloc(cap + (size_t)limit * tl);
    char* w = out; char* p = s; char* m;
    while (n < (size_t)limit && (m = strstr(p, from)) != NULL) {
        memcpy(w, p, (size_t)(m - p)); w += m - p;
        memcpy(w, to, tl); w += tl;
        p = m + fl; n++;
    }
    strcpy(w, p);
    return out;
}
char* moira_str_replace(char* s, char* from, char* to) {
    size_t fl = strlen(from), tl = strlen(to), cnt = 0;
    if (fl == 0) return moira_str_dup(s);
    for (char* p = s; (p = strstr(p, from)); p += fl) cnt++;
    size_t len = strlen(s) - cnt * fl + cnt * tl;
    char* r = (char*)malloc(len + 1);
    char* w = r;
    char* p = s;
    char* m;
    while ((m = strstr(p, from)) != NULL) {
        memcpy(w, p, (size_t)(m - p)); w += m - p;
        memcpy(w, to, tl); w += tl;
        p = m + fl;
    }
    strcpy(w, p);
    return r;
}
"#;

/// Emit a whole program as a single C translation unit.
///
/// Struct definitions are emitted sequentially (they are few and each function
/// body may reference them), but the function bodies are emitted **in parallel**
/// — each function is independent (bodies reference one another only by name,
/// resolved by the C compiler) so the per-function emission passes run
/// concurrently on separate OS threads. The results are joined in the original
/// function order so the emitted C stays byte-identical across runs (the
/// cross-stage determinism guarantee in `tests/determinism.rs`); this mirrors
/// the type-check / borrow-check parallelization in `tpt_moira_lachesis::check`.
/// Names that cannot be used as C identifiers here: the C keywords, and the libc names the
/// emitted prelude declares (a Rust function `exit` would clash with `stdlib.h`).
const C_RESERVED: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else",
    "enum", "extern", "float", "for", "goto", "if", "inline", "int", "long", "register",
    "restrict", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef",
    "union", "unsigned", "void", "volatile", "while", "_Bool", "NULL", "EOF", "stdin", "stdout",
    "stderr", "errno", "abs", "labs", "exit", "abort", "malloc", "calloc", "realloc", "free",
    "printf", "fprintf", "sprintf", "puts", "fputs", "putchar", "getchar", "strlen", "strcmp",
    "strcpy", "strncmp", "strstr", "strchr", "memcpy", "memset", "memcmp", "isspace", "isalpha",
    "isdigit", "isalnum", "isxdigit", "isupper", "islower", "tolower", "toupper", "sqrt", "pow",
    "rand", "time", "clock", "assert", "remove", "rename", "system", "atoi", "atol", "strtol",
    // POSIX names that the C headers declare (`dirent.h` and MinGW's `io.h` declare `open`).
    "open", "close", "read", "write", "access", "stat", "opendir", "readdir", "closedir", "unlink",
    "mkdir", "getenv", "fopen", "fclose", "fread", "fwrite", "feof", "ferror",
];

/// The C spelling of a MIR name: a C reserved word gets a trailing `_`.
fn c_name(name: &str) -> Option<String> {
    if name != "main" && C_RESERVED.contains(&name) {
        Some(format!("{name}_"))
    } else {
        None
    }
}

/// Rename the C-reserved names of `prog` (functions, parameters, locals, temporaries and
/// the operands that refer to them) so the emitted C is valid. A name that is not reserved
/// is left as it is.
fn escape_c_names(prog: &mir::MirProgram) -> mir::MirProgram {
    let mut prog = prog.clone();
    let defined: std::collections::HashSet<String> =
        prog.functions.iter().map(|f| f.name.clone()).collect();
    for f in prog.functions.iter_mut() {
        escape_function(f, &defined);
    }
    prog
}

// Index loops, not `iter_mut()`: the Stage-1 subset lowers `for` only over ranges and arrays.
#[allow(clippy::needless_range_loop)]
fn escape_function(f: &mut mir::MirFunction, defined: &std::collections::HashSet<String>) {
    if let Some(c) = c_name(&f.name) {
        f.name = c;
    }
    for i in 0..f.params.len() {
        escape_name(&mut f.params[i]);
    }
    for i in 0..f.locals.len() {
        escape_name(&mut f.locals[i].0);
    }
    for i in 0..C_RESERVED.len() {
        let n = C_RESERVED[i];
        if f.temp_types.contains_key(n) {
            let ty = f.temp_types.get(n).cloned().unwrap_or_default();
            f.temp_types.remove(n);
            f.temp_types.insert(format!("{n}_"), ty);
        }
    }
    for bi in 0..f.blocks.len() {
        for si in 0..f.blocks[bi].stmts.len() {
            escape_name(&mut f.blocks[bi].stmts[si].place);
            escape_rvalue(&mut f.blocks[bi].stmts[si].rvalue, defined);
        }
        let b = &mut f.blocks[bi];
        match &mut b.term {
            mir::Terminator::If(op, _, _) | mir::Terminator::Switch(op, _, _) => {
                escape_op(op, defined)
            }
            mir::Terminator::Return(Some(op)) => escape_op(op, defined),
            _ => {}
        }
    }
}

fn escape_name(n: &mut String) {
    if let Some(c) = c_name(n) {
        *n = c;
    }
}

/// A variable is always renamed; a call target only when the program defines that
/// function. A library call the lowering emits (`strlen`, `exit`) keeps its C name.
fn escape_op(op: &mut mir::Operand, defined: &std::collections::HashSet<String>) {
    match op {
        mir::Operand::Var(n) => escape_name(n),
        mir::Operand::Func(n) if defined.contains(n.as_str()) => escape_name(n),
        _ => {}
    }
}

// Index loops, not `iter_mut()`: the Stage-1 subset lowers `for` only over ranges and arrays.
#[allow(clippy::needless_range_loop)]
fn escape_rvalue(rv: &mut mir::Rvalue, defined: &std::collections::HashSet<String>) {
    match rv {
        mir::Rvalue::Use(a)
        | mir::Rvalue::Unary(_, a)
        | mir::Rvalue::Field(a, _, _)
        | mir::Rvalue::FieldAddr(a, _, _)
        | mir::Rvalue::Ref(a, _)
        | mir::Rvalue::Cast(a, _) => escape_op(a, defined),
        mir::Rvalue::Binary(_, a, b) | mir::Rvalue::Index(a, b) | mir::Rvalue::IndexRef(a, b) => {
            escape_op(a, defined);
            escape_op(b, defined);
        }
        mir::Rvalue::Call(c, args) => {
            escape_op(c, defined);
            for i in 0..args.len() {
                escape_op(&mut args[i], defined);
            }
        }
        mir::Rvalue::StructLit(_, fields) => {
            for i in 0..fields.len() {
                escape_op(&mut fields[i].1, defined);
            }
        }
        mir::Rvalue::StoreIndex(a, b, c) => {
            escape_op(a, defined);
            escape_op(b, defined);
            escape_op(c, defined);
        }
        mir::Rvalue::StoreField(a, _, b, _) => {
            escape_op(a, defined);
            escape_op(b, defined);
        }
        mir::Rvalue::Array(_, ops) | mir::Rvalue::Tuple(_, ops) => {
            for i in 0..ops.len() {
                escape_op(&mut ops[i], defined);
            }
        }
    }
}

pub fn emit_c(prog: &mir::MirProgram) -> String {
    let prog = &crate::retype::retype_temps(&escape_c_names(prog));
    let mut out = String::new();
    out.push_str("/* Generated by Moira (Atropos Stage-0 C backend) */\n");
    out.push_str(
        "#include <stdio.h>\n#include <stdint.h>\n#include <stdlib.h>\n#include <string.h>\n\n",
    );
    // Built-ins used by the Stage-0 subset for observable output during testing.
    out.push_str("void print_i32(long long x) { printf(\"%lld\\n\", x); }\n");
    out.push_str("void print_i64(long long x) { printf(\"%lld\\n\", x); }\n");
    out.push_str("void print_usize(unsigned long long x) { printf(\"%llu\\n\", x); }\n");
    out.push_str(F64_RUNTIME);
    out.push_str(
        "void print_f64(double x) { char b[400]; moira_f64_format(b, x); printf(\"%s\\n\", b); }\n",
    );
    out.push_str("void print_bool(long long x) { printf(\"%s\\n\", x ? \"true\" : \"false\"); }\n");
    out.push_str("void print_str(char* x) { printf(\"%s\\n\", x); }\n");
    // Raw writers (no newline): `print!`/`println!` write their pieces with these and
    // end the line with an explicit "\n" piece.
    out.push_str("void __moira_w_i64(long long x) { printf(\"%lld\", x); }\n");
    out.push_str("void __moira_w_usize(unsigned long long x) { printf(\"%llu\", x); }\n");
    out.push_str("void __moira_w_f64(double x) { char b[400]; moira_f64_format(b, x); printf(\"%s\", b); }\n");
    out.push_str(
        "void __moira_w_bool(long long x) { printf(\"%s\", x ? \"true\" : \"false\"); }\n",
    );
    out.push_str("void __moira_w_str(char* x) { printf(\"%s\", x); }\n");
    // Failed `assert!`/`panic!`: report on stderr and exit with status 101.
    out.push_str("void __moira_panic(void) { fputs(\"moira: panic\\n\", stderr); exit(101); }\n\n");
    // A string slice `s[a..b]` is a new NUL-terminated copy of the bytes from `a` to `b`.
    out.push_str("char* moira_str_sub(char* s, long long a, long long b) { long long n = b - a; char* r = (char*)malloc((size_t)n + 1); memcpy(r, s + a, (size_t)n); r[n] = 0; return r; }\n\n");
    out.push_str(STR_RUNTIME);
    // Instances of the generic types (`Option<T>`, `Result<T,E>`, tuples) and the hash
    // maps the program uses get their structs and helpers.
    let (extra_structs, map_types) = instantiations(prog);
    let mut owned_prog = prog.clone();
    owned_prog.structs.extend(extra_structs);
    let prog = &owned_prog;

    // Names of all struct types so that struct-typed fields/params can be
    // emitted with the required `struct` keyword in C.
    let struct_names: std::collections::HashSet<&str> =
        prog.structs.iter().map(|s| s.name.as_str()).collect();

    // Names of functions that return `void` (so a call may not be assigned to a
    // temporary — doing so is a hard C error). User functions with `ret == None`
    // plus the builtin `print_*` helpers.
    let mut void_names: std::collections::HashSet<&str> = prog
        .functions
        .iter()
        .filter(|f| f.ret.is_none())
        .map(|f| f.name.as_str())
        .collect();
    for b in [
        "print_i32",
        "print_i64",
        "print_usize",
        "print_f64",
        "print_bool",
        "print_str",
        "__moira_w_i64",
        "__moira_w_usize",
        "__moira_w_f64",
        "__moira_w_bool",
        "__moira_w_str",
        "__moira_panic",
    ] {
        void_names.insert(b);
    }

    // Element types of every `Vec<T>` used anywhere in the program. Each gets a
    // generated struct (typedef'd before the user structs, so a struct may hold a
    // vector) plus helper functions emitted after the struct definitions.
    let mut vec_elems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut slice_elems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for s in &prog.structs {
        for (_, fty) in &s.fields {
            collect_vec_elems(fty, &mut vec_elems, &mut slice_elems);
        }
    }
    for f in &prog.functions {
        if let Some(r) = &f.ret {
            collect_vec_elems(r, &mut vec_elems, &mut slice_elems);
        }
        for (_, t) in &f.locals {
            if let Some(t) = t {
                collect_vec_elems(t, &mut vec_elems, &mut slice_elems);
            }
        }
        for t in f.temp_types.values() {
            collect_vec_elems(t, &mut vec_elems, &mut slice_elems);
        }
    }
    // Maps are referenced by pointer from vectors and slices, so each map name is declared
    // before any of them; its definition follows.
    for (k, v) in &map_types {
        let name = mir::map_c_name(k, v);
        out.push_str(&format!(
            "typedef struct {name} {name};
"
        ));
    }
    // Inner element types first: a nested vector's typedef needs its element's.
    let mut ordered: Vec<&String> = vec_elems.iter().collect();
    ordered.sort_by_key(|e| e.split("Vec<").count() - 1);
    for elem in &ordered {
        out.push_str(&vec_typedef(elem, &struct_names));
    }
    for elem in &slice_elems {
        out.push_str(&slice_typedef(elem, &struct_names));
    }
    for (k, v) in &map_types {
        out.push_str(&map_typedef(k, v, &struct_names));
    }
    if !vec_elems.is_empty() {
        out.push('\n');
    }

    for &i in &struct_order(prog) {
        let s = &prog.structs[i];
        out.push_str(&format!("struct {} {{\n", s.name));
        for (fname, fty) in &s.fields {
            out.push_str(&format!("    {} {};\n", c_type(fty, &struct_names), fname));
        }
        out.push_str("};\n\n");
    }

    for elem in &vec_elems {
        out.push_str(&vec_helpers(elem, &struct_names));
    }
    for elem in &slice_elems {
        out.push_str(&slice_helpers(elem, &struct_names));
    }
    for (k, v) in &map_types {
        out.push_str(&map_helpers(k, v, &struct_names, &prog.structs));
    }

    // Prototypes first: a function may be called before it is defined (forward
    // references among top-level functions, and hoisted nested items).
    for f in &prog.functions {
        out.push_str(&c_signature(f, &struct_names));
        out.push_str(";\n");
    }
    out.push('\n');

    // Emit each function body in order. (The bodies were once emitted on threads;
    // the subset has no threads, and the output is the same either way.)
    let bodies: Vec<String> = prog
        .functions
        .iter()
        .map(|f| emit_function(f, &struct_names, &void_names))
        .collect();
    for b in bodies {
        out.push_str(&b);
        out.push('\n');
    }

    mark_runtime_static(&out)
}

/// Make the runtime helpers (`moira_*`, `print_*`, `__moira_*`) file-local. Every program carries
/// the whole runtime, and a non-`static` helper is compiled at full optimisation even when the
/// program never calls it; `static` lets the C compiler drop the unused ones before optimising.
/// Only top-level definitions and their prototypes are changed (a header line ends in `{`, `}` or `;`), so
/// prototypes and user functions are left alone.
fn mark_runtime_static(c: &str) -> String {
    const PREFIXES: [&str; 3] = ["moira_", "print_", "__moira_"];
    let mut out = String::with_capacity(c.len() + 512);
    for line in c.split_inclusive('\n') {
        let body = line.trim_end();
        let top_level = !body.is_empty() && !body.starts_with([' ', '\t', '#', '}']);
        // Prototypes are marked too, so a helper's declaration and definition agree (a `static`
        // definition after a plain prototype is a C error; the lifted closures are declared so).
        if top_level
            && (body.ends_with('{') || body.ends_with('}') || body.ends_with(';'))
            && !body.starts_with("static ")
            && !body.starts_with("typedef")
        {
            if let Some(paren) = body.find('(') {
                let head = &body[..paren];
                let name = head
                    .rsplit(' ')
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('*');
                if PREFIXES.iter().any(|p| name.starts_with(p)) {
                    out.push_str("static ");
                }
            }
        }
        out.push_str(line);
    }
    out
}

/// The C signature of a function (`ret name(params)`, without the body).
fn c_signature(f: &mir::MirFunction, struct_names: &std::collections::HashSet<&str>) -> String {
    let ret = if f.name == "main" {
        "int".to_string()
    } else {
        f.ret
            .clone()
            .as_deref()
            .map(|t| c_type(t, struct_names))
            .unwrap_or_else(|| "void".to_string())
    };
    let mut param_types: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (name, ty) in &f.locals {
        if let Some(t) = ty {
            param_types.insert(name, t);
        }
    }
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| {
            let t = param_types.get(p.as_str()).copied().unwrap_or("long long");
            c_decl(t, p, struct_names)
        })
        .collect();
    // `main` takes the process arguments (kept for `env::args()`).
    let param_str = if params.is_empty() && f.name == "main" {
        "int argc, char** argv".to_string()
    } else if params.is_empty() {
        "void".to_string()
    } else {
        params.join(", ")
    };
    fn_header(&ret, &f.name, &param_str)
}

/// The header `ret name(params)` of a C function. A function-pointer return type
/// (`R (*)(P)`) cannot be written as a prefix: the declarator must wrap the name and
/// parameter list, `R (*name(params))(P)`.
fn fn_header(ret: &str, name: &str, param_str: &str) -> String {
    if ret.contains("(*)") {
        ret.replacen("(*)", &format!("(*{}({}))", name, param_str), 1)
    } else {
        format!("{} {}({})", ret, name, param_str)
    }
}

/// The declared type of a temporary or local of `f`, if known.
fn dest_type(f: &mir::MirFunction, place: &str) -> Option<String> {
    f.temp_types.get(place).cloned().or_else(|| {
        f.locals
            .iter()
            .find(|(n, _)| n == place)
            .and_then(|(_, t)| t.clone())
    })
}

/// A struct literal of a prelude enum (`Option`, `Result`) names the instance struct of
/// its destination's type (`Option<i32>` is `Option_i32`); other literals are unchanged.
fn fix_struct_lit(f: &mir::MirFunction, place: &str, rv: &mir::Rvalue) -> mir::Rvalue {
    if let mir::Rvalue::StructLit(name, fields) = rv {
        if let Some(dest) = dest_type(f, place) {
            if let Some((base, _)) = mir::split_generic(&dest) {
                if base == name {
                    return mir::Rvalue::StructLit(mir::inst_c_name(&dest), fields.clone());
                }
            }
        }
    }
    rv.clone()
}

fn emit_function(
    f: &mir::MirFunction,
    struct_names: &std::collections::HashSet<&str>,
    void_names: &std::collections::HashSet<&str>,
) -> String {
    let ret = if f.name == "main" {
        "int".to_string()
    } else {
        f.ret
            .clone()
            .as_deref()
            .map(|t| c_type(t, struct_names))
            .unwrap_or_else(|| "void".to_string())
    };

    let mut param_types: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (name, ty) in &f.locals {
        if let Some(t) = ty {
            param_types.insert(name, t);
        }
    }
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| {
            let t = param_types.get(p.as_str()).copied().unwrap_or("long long");
            c_decl(t, p, struct_names).to_string()
        })
        .collect();
    // `main` takes the process arguments (kept for `env::args()`).
    let is_main_args = params.is_empty() && f.name == "main";
    let param_str = if is_main_args {
        "int argc, char** argv".to_string()
    } else if params.is_empty() {
        "void".to_string()
    } else {
        params.join(", ")
    };

    let mut out = format!("{} {{\n", fn_header(&ret, &f.name, &param_str));
    if is_main_args {
        out.push_str("    moira_argc = argc;\n    moira_argv = argv;\n");
    }

    // Local variable declarations. Skip locals that are also parameters or that
    // have already been declared (the async poll path may register a slot as
    // both a pre-declared local and again when lowering its `let`), to avoid a C
    // redeclaration error.
    let mut declared: std::collections::HashSet<&str> =
        f.params.iter().map(|p| p.as_str()).collect();
    for (name, ty) in &f.locals {
        if declared.contains(name.as_str()) {
            continue;
        }
        declared.insert(name.as_str());
        let t = ty
            .clone()
            .as_deref()
            .map(|t| c_type(t, struct_names))
            .unwrap_or_else(|| "long long".to_string());
        out.push_str(&format!("    {};\n", c_decl(&t, name, struct_names)));
    }
    // SSA temporary declarations. Iterate in a deterministic (sorted) order so
    // the emitted C is identical across runs/processes — `temp_types` is a
    // `HashMap`, whose iteration order is otherwise randomized per process and
    // would break stage-1 == stage-2 output parity. Names already declared as
    // parameters or locals are skipped: `lower` publishes local types into
    // `temp_types` too, and a duplicate `struct Point p;` would not compile.
    let mut temps: Vec<(&String, &String)> = f.temp_types.iter().collect();
    temps.sort_by(|a, b| a.0.cmp(b.0));
    for (name, ty) in temps {
        if declared.contains(name.as_str()) {
            continue;
        }
        out.push_str(&format!("    {};\n", c_decl(ty, name, struct_names)));
    }
    out.push('\n');

    for (i, block) in f.blocks.iter().enumerate() {
        out.push_str(&format!("b{}:\n", i));
        for s in &block.stmts {
            match &s.rvalue {
                // StoreField renders its own full assignment (no destination).
                mir::Rvalue::StoreField(..) | mir::Rvalue::StoreIndex(..) => {
                    out.push_str(&format!("    {};\n", emit_rvalue(&s.rvalue)));
                }
                // An array literal is declared inline with its initializer
                // (`elem_ty name[len] = {..};`) — C does not allow assigning to
                // an array, so it cannot use the ordinary `place = rhs;` form.
                mir::Rvalue::Array(elem_ty, elems) => {
                    let inits: Vec<String> = elems.iter().map(emit_operand).collect();
                    out.push_str(&format!(
                        "    {} {}[{}] = {{ {} }};\n",
                        c_type(elem_ty, struct_names),
                        s.place,
                        elems.len(),
                        inits.join(", ")
                    ));
                }
                // A void call is emitted as a bare statement (no destination).
                mir::Rvalue::Call(mir::Operand::Func(n), _) if void_names.contains(n.as_str()) => {
                    out.push_str(&format!("    {};\n", emit_rvalue(&s.rvalue)));
                }
                // `Box::new(v)` (lowered to `moira_box_new(v)`): a fresh heap copy of `v`,
                // typed by the destination's pointee.
                mir::Rvalue::Call(mir::Operand::Func(n), args)
                    if n == "moira_box_new" && args.len() == 1 =>
                {
                    let pointee = dest_type(f, &s.place)
                        .and_then(|d| d.strip_suffix('*').map(|p| p.to_string()))
                        .unwrap_or_else(|| "long long".to_string());
                    let ct = c_type(&pointee, struct_names);
                    out.push_str(&format!(
                        "    {} = ({}*)malloc(sizeof({}));\n",
                        s.place, ct, ct
                    ));
                    out.push_str(&format!("    *{} = {};\n", s.place, emit_operand(&args[0])));
                }
                _ => {
                    let rv = fix_struct_lit(f, &s.place, &s.rvalue);
                    out.push_str(&format!("    {} = {};\n", s.place, emit_rvalue(&rv)));
                }
            }
        }
        // `main` is declared `int`, so a valueless return must still yield a status.
        if f.name == "main" && matches!(block.term, mir::Terminator::Return(None)) {
            out.push_str("    return 0;\n");
        } else {
            out.push_str(&emit_terminator(&block.term));
        }
        out.push('\n');
    }

    out.push_str("}\n");
    out
}

fn emit_operand(o: &mir::Operand) -> String {
    match o {
        mir::Operand::Var(n) => n.clone(),
        mir::Operand::Const(c) => c.to_string(),
        mir::Operand::Float(b) => c_f64_lit(mir::Operand::as_f64(*b)),
        mir::Operand::Unit => "0".to_string(),
        mir::Operand::Func(n) => n.clone(),
        mir::Operand::CStr(s) => c_str_lit(s),
    }
}

/// A C `double` literal with the exact value. `{:?}` is the shortest round-trip form and
/// always has a `.` or an exponent, so C reads it as floating-point, not as an integer.
fn c_f64_lit(v: f64) -> String {
    if v.is_nan() {
        "(0.0/0.0)".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "(1.0/0.0)" } else { "(-1.0/0.0)" }.to_string()
    } else {
        format!("{v:?}")
    }
}

/// Render a Rust string as a C string literal (escapes `\`, `"`, and the
/// common control characters).
fn c_str_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Map a Moira primitive type name to its C equivalent. Struct type names and
/// unknown names pass through unchanged.
/// The indices of `prog.structs` in an order where every struct comes after the
/// structs its fields hold by value (a pointer field needs no definition first).
fn struct_order(prog: &mir::MirProgram) -> Vec<usize> {
    fn visit(
        i: usize,
        prog: &mir::MirProgram,
        index: &std::collections::HashMap<&str, usize>,
        state: &mut Vec<u8>,
        order: &mut Vec<usize>,
    ) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for (_, fty) in &prog.structs[i].fields {
            if fty.ends_with('*') {
                continue;
            }
            // A generic instance is the struct of its instance name.
            let key = if mir::split_generic(fty).is_some() {
                mir::inst_c_name(fty)
            } else {
                fty.clone()
            };
            if let Some(&j) = index.get(key.as_str()) {
                visit(j, prog, index, state, order);
            }
        }
        state[i] = 2;
        order.push(i);
    }
    let index: std::collections::HashMap<&str, usize> = prog
        .structs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();
    let mut state = vec![0u8; prog.structs.len()];
    let mut order = Vec::new();
    for i in 0..prog.structs.len() {
        visit(i, prog, &index, &mut state, &mut order);
    }
    order
}

/// Record the element type of every `Vec<T>` (into `vecs`) and `Slice<T>` (into
/// `slices`) mentioned in the type string `ty`.
fn collect_vec_elems(
    ty: &str,
    vecs: &mut std::collections::BTreeSet<String>,
    slices: &mut std::collections::BTreeSet<String>,
) {
    collect_seq(ty, "Vec<", vecs);
    collect_seq(ty, "Slice<", slices);
}

/// Record the element type of every `prefix…>` sequence type mentioned in `ty`.
fn collect_seq(ty: &str, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
    let mut rest = ty;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        // Find the `>` that closes this `Vec<`, accounting for nesting.
        let mut depth = 1usize;
        let mut end = None;
        for (i, ch) in after.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => {
                out.insert(after[..e].to_string());
                // A nested sequence (`Vec<Vec<T>>`) also records its inner element type.
                collect_seq(&after[..e], prefix, out);
                rest = &after[e + 1..];
            }
            None => break,
        }
    }
}

/// The typedef of the generated `Slice<elem>` struct: a view of `data` and `len`.
fn slice_typedef(elem: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    let name = mir::slice_c_name(elem);
    let ec = c_type(elem, struct_names);
    format!(
        "typedef struct {name} {{ {ec}* data; long long len; }} {name};
"
    )
}

/// The helper of the generated `Slice<elem>` type: `sub(data, len, a, b)` is the
/// sub-slice `data[a..b]`.
fn slice_helpers(elem: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    let name = mir::slice_c_name(elem);
    let ec = c_type(elem, struct_names);
    let sub = mir::slice_fn_name(elem, "sub");
    let contains =
        contains_helper(&name, &ec, &mir::slice_fn_name(elem, "contains")).unwrap_or_default();
    // A byte slice can view a string's bytes: `from_str(s)` aliases the NUL-terminated text.
    let from_str = if elem == "u8" {
        let f = mir::slice_fn_name(elem, "from_str");
        format!(
            "static {name} {f}(char* s) {{ {name} r; r.data = (uint8_t*)s; r.len = (long long)strlen(s); return r; }}
"
        )
    } else {
        String::new()
    };
    format!(
        "static {name} {sub}({ec}* data, long long len, long long a, long long b) {{ {name} s; s.data = data + a; s.len = b - a; (void)len; return s; }}
{from_str}{contains}
"
    )
}

/// The `contains(v, x)` helper of a sequence struct `name` (a vector or a slice: both hold
/// `data` and `len`), for elements that compare: strings by content, scalars with `==`.
fn contains_helper(name: &str, ec: &str, fn_name: &str) -> Option<String> {
    if ec == "char*" {
        return Some(format!(
            "static long long {fn_name}({name}* v, char* x) {{ for (long long i = 0; i < v->len; i++) if (strcmp(v->data[i], x) == 0) return 1; return 0; }}
"
        ));
    }
    // Only C arithmetic types compare with `==`: a struct (a vector, an enum) does not.
    let scalar = matches!(
        ec,
        "long long"
            | "unsigned long long"
            | "int"
            | "unsigned int"
            | "short"
            | "unsigned short"
            | "char"
            | "unsigned char"
            | "signed char"
            | "_Bool"
            | "bool"
            | "double"
            | "float"
            | "int64_t"
            | "uint64_t"
            | "int32_t"
            | "uint32_t"
            | "int16_t"
            | "uint16_t"
            | "int8_t"
            | "uint8_t"
            | "size_t"
    );
    if !scalar {
        return None;
    }
    Some(format!(
        "static long long {fn_name}({name}* v, {ec} x) {{ for (long long i = 0; i < v->len; i++) if (v->data[i] == x) return 1; return 0; }}
"
    ))
}

/// The typedef of the generated `Vec<elem>` struct.
fn vec_typedef(elem: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    let name = mir::vec_c_name(elem);
    let ec = c_type(elem, struct_names);
    format!("typedef struct {name} {{ {ec}* data; long long len; long long cap; }} {name};\n")
}

/// The helper functions of the generated `Vec<elem>` type: `new`, `push`
/// (grows by doubling), `len` and `is_empty`.
fn vec_helpers(elem: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    let name = mir::vec_c_name(elem);
    let ec = c_type(elem, struct_names);
    let new = mir::vec_fn_name(elem, "new");
    let push = mir::vec_fn_name(elem, "push");
    let len = mir::vec_fn_name(elem, "len");
    let is_empty = mir::vec_fn_name(elem, "is_empty");
    let mut s = String::new();
    s.push_str(&format!(
        "static {name} {new}(void) {{ {name} v; v.data = NULL; v.len = 0; v.cap = 0; return v; }}\n"
    ));
    s.push_str(&format!("static long long {push}({name}* v, {ec} x) {{\n"));
    s.push_str("    if (v->len == v->cap) {\n");
    s.push_str("        long long nc = v->cap ? v->cap * 2 : 4;\n");
    s.push_str(&format!(
        "        v->data = ({ec}*)realloc(v->data, (size_t)nc * sizeof({ec}));\n"
    ));
    s.push_str("        v->cap = nc;\n");
    s.push_str("    }\n");
    s.push_str("    v->data[v->len++] = x;\n");
    s.push_str("    return 0;\n");
    s.push_str("}\n");
    s.push_str(&format!(
        "static long long {len}({name}* v) {{ return v->len; }}\n"
    ));
    s.push_str(&format!(
        "static long long {is_empty}({name}* v) {{ return v->len == 0; }}\n"
    ));
    // `to_vec` of a slice or vector: a new vector of the first `n` elements. String elements are
    // duplicated; other elements are copied by value.
    let copy_n = mir::vec_fn_name(elem, "copy_n");
    let elem_copy = if ec == "char*" {
        "moira_str_dup(data[i])"
    } else {
        "data[i]"
    };
    s.push_str(&format!(
        "static {name} {copy_n}({ec}* data, long long n) {{ {name} v = {new}(); for (long long i = 0; i < n; i++) {push}(&v, {elem_copy}); return v; }}
"
    ));
    // In-place operations: `reverse` swaps the ends inward, `remove(i)` shifts the tail left
    // and returns the element (out of range panics, as in Rust), and `append(w)` moves `w`'s
    // elements to the end of `v`, leaving `w` empty. Each returns a status or the element.
    let reverse = mir::vec_fn_name(elem, "reverse");
    let remove = mir::vec_fn_name(elem, "remove");
    let append = mir::vec_fn_name(elem, "append");
    s.push_str(&format!(
        "static long long {reverse}({name}* v) {{ for (long long i = 0, j = v->len - 1; i < j; i++, j--) {{ {ec} t = v->data[i]; v->data[i] = v->data[j]; v->data[j] = t; }} return 0; }}\n"
    ));
    s.push_str(&format!(
        "static {ec} {remove}({name}* v, long long i) {{ if (i < 0 || i >= v->len) __moira_panic(); {ec} x = v->data[i]; memmove(&v->data[i], &v->data[i + 1], (size_t)(v->len - i - 1) * sizeof({ec})); v->len--; return x; }}\n"
    ));
    s.push_str(&format!(
        "static long long {append}({name}* v, {name}* w) {{ for (long long i = 0; i < w->len; i++) {push}(v, w->data[i]); w->len = 0; return 0; }}\n"
    ));
    // `insert(i, x)`: the element goes at index `i`, after the tail moves right by one. An index
    // past the end panics, as in Rust.
    let insert = mir::vec_fn_name(elem, "insert");
    s.push_str(&format!(
        "static long long {insert}({name}* v, long long i, {ec} x) {{ if (i < 0 || i > v->len) __moira_panic(); {push}(v, x); memmove(&v->data[i + 1], &v->data[i], (size_t)(v->len - 1 - i) * sizeof({ec})); v->data[i] = x; return 0; }}\n"
    ));
    // `clear` empties the vector; `dedup` drops each item equal to the one before it (strings by
    // content, scalars with `==`). Other element types have no equality, so no `dedup`.
    let clear = mir::vec_fn_name(elem, "clear");
    s.push_str(&format!(
        "static long long {clear}({name}* v) {{ v->len = 0; return 0; }}
"
    ));
    let is_scalar = matches!(
        ec.as_str(),
        "long long"
            | "unsigned long long"
            | "int"
            | "unsigned int"
            | "short"
            | "unsigned short"
            | "char"
            | "unsigned char"
            | "signed char"
            | "_Bool"
            | "bool"
            | "double"
            | "float"
            | "int64_t"
            | "uint64_t"
            | "int32_t"
            | "uint32_t"
            | "int16_t"
            | "uint16_t"
            | "int8_t"
            | "uint8_t"
    );
    if ec == "char*" || is_scalar {
        let dedup = mir::vec_fn_name(elem, "dedup");
        let same = if ec == "char*" {
            "strcmp(v->data[i], v->data[w - 1]) == 0"
        } else {
            "v->data[i] == v->data[w - 1]"
        };
        s.push_str(&format!(
            "static long long {dedup}({name}* v) {{ if (v->len == 0) return 0; long long w = 1; for (long long i = 1; i < v->len; i++) if (!({same})) v->data[w++] = v->data[i]; v->len = w; return 0; }}
"
        ));
    }
    // `contains`: strings compare by content, scalars with `==`; other element types have no
    // equality helper yet.
    if let Some(body) = contains_helper(&name, &ec, &mir::vec_fn_name(elem, "contains")) {
        s.push_str(&body);
    }
    s.push('\n');
    s
}

fn c_type(ty: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    // A bare `Result` is `fmt::Result` (the checker's type of `write!`): a status code, as the
    // lowering models it. The prelude `Result` enum is only ever instantiated with arguments.
    if ty == "Result" {
        return "long long".to_string();
    }
    if let Some(elem) = mir::slice_elem(ty) {
        return mir::slice_c_name(elem);
    }
    // A function type `Fn<params..,result>` is a function pointer with that signature.
    if let Some(("Fn", parts)) = mir::split_generic(ty) {
        if let Some(ret) = parts.last() {
            let params = &parts[..parts.len() - 1];
            let ret_c = c_type(ret, struct_names);
            let params_c: Vec<String> = params.iter().map(|p| c_type(p, struct_names)).collect();
            let list = if params_c.is_empty() {
                "void".to_string()
            } else {
                params_c.join(", ")
            };
            return format!("{ret_c} (*)({list})");
        }
    }
    if let Some((base, args)) = mir::split_generic(ty) {
        match (base, args.len()) {
            ("Map" | "Sorted", 2) => return mir::map_c_name(args[0], args[1]),
            // A set is a map to `bool`.
            ("Set" | "SortedSet", 1) => return mir::map_c_name(args[0], "bool"),
            ("Option" | "Result" | "Tuple", _) => {
                let name = mir::inst_c_name(ty);
                return if struct_names.contains(name.as_str()) {
                    format!("struct {name}")
                } else {
                    name
                };
            }
            _ => {}
        }
    }
    // `Vec<T>` is a generated struct; `Vec<T>*` is a pointer to it.
    if let Some(base) = ty.strip_suffix('*') {
        return format!("{}*", c_type(base, struct_names));
    }
    if let Some(elem) = mir::vec_elem(ty) {
        return mir::vec_c_name(elem);
    }
    // A struct-typed field/param must be emitted with the `struct` keyword in
    // C (it is not a typedef). Pointers to structs are handled by the recursion above.
    if struct_names.contains(ty) {
        return format!("struct {}", ty);
    }
    let mapped = match ty {
        "i8" => "int8_t",
        "i16" => "int16_t",
        "i32" => "int32_t",
        // `i64` is spelled `long long`, the same type as the default integer and the runtime
        // helpers. `int64_t` is `long` on LP64 hosts, so mixing the two made pointers such as
        // `int64_t*` and `long long*` incompatible there (a hard error on GCC 14+).
        "i64" => "long long",
        "i128" => "long long",
        "isize" => "long long",
        "u8" => "uint8_t",
        "u16" => "uint16_t",
        "u32" => "uint32_t",
        "u64" => "uint64_t",
        "u128" => "unsigned long long",
        "usize" => "unsigned long long",
        "bool" => "int",
        "char" => "char",
        "f32" => "float",
        "f64" => "double",
        "str" => "char*",
        // A `Formatter` is the string buffer it writes into (see `lower::Lower::user_display_piece`).
        "fmtbuf" => "char*",
        other => other,
    };
    mapped.to_string()
}

/// Render a *declaration* of `name` with C type `ty`.
///
/// Most types lower to `ctypename name`, but a function-pointer type such as
/// `long long (*)(long long)` cannot be written that way in C — the variable name
/// must sit inside the parentheses: `long long (*name)(long long)`. This helper
/// rewrites the `(*)` to `(*name)` for such types and otherwise defers to
/// [`c_type`].
fn c_decl(ty: &str, name: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    // A function pointer's name sits inside its parentheses: `R (*name)(P)`.
    let cty = c_type(ty, struct_names);
    if let Some(star) = cty.find("(*)") {
        let (ret, rest) = cty.split_at(star);
        let rest = rest.replacen("(*)", &format!("(*{})", name), 1);
        return format!("{}{}", ret.trim_end(), rest);
    }
    format!("{} {}", cty, name)
}

fn emit_rvalue(rv: &mir::Rvalue) -> String {
    match rv {
        mir::Rvalue::Use(o) => emit_operand(o),
        mir::Rvalue::Binary(op, l, r) => {
            format!("({} {} {})", emit_operand(l), c_binop(*op), emit_operand(r))
        }
        mir::Rvalue::Unary(op, o) => match op {
            mir::MirUnOp::Neg => format!("(-{})", emit_operand(o)),
            mir::MirUnOp::Not => format!("(!{})", emit_operand(o)),
            mir::MirUnOp::Deref => format!("(*{})", emit_operand(o)),
        },
        mir::Rvalue::Call(callee, args) => {
            let a: Vec<String> = args.iter().map(emit_operand).collect();
            format!("{}({})", emit_operand(callee), a.join(", "))
        }
        mir::Rvalue::StructLit(name, fields) => {
            let inits: Vec<String> = fields
                .iter()
                .map(|(f, v)| format!(".{} = {}", f, emit_operand(v)))
                .collect();
            format!("(struct {}){{ {} }}", name, inits.join(", "))
        }
        mir::Rvalue::Field(o, f, deref) => {
            if *deref {
                format!("{}->{}", emit_operand(o), f)
            } else {
                format!("{}.{}", emit_operand(o), f)
            }
        }
        mir::Rvalue::FieldAddr(o, f, deref) => {
            if *deref {
                format!("&{}->{}", emit_operand(o), f)
            } else {
                format!("&{}.{}", emit_operand(o), f)
            }
        }
        mir::Rvalue::Ref(o, _) => format!("&{}", emit_operand(o)),
        mir::Rvalue::Index(b, i) => format!("{}[{}]", emit_operand(b), emit_operand(i)),
        mir::Rvalue::IndexRef(b, i) => format!("&{}[{}]", emit_operand(b), emit_operand(i)),
        mir::Rvalue::StoreIndex(b, i, v) => {
            format!(
                "{}[{}] = {}",
                emit_operand(b),
                emit_operand(i),
                emit_operand(v)
            )
        }
        mir::Rvalue::Tuple(ty, elems) => {
            let inits: Vec<String> = elems
                .iter()
                .enumerate()
                .map(|(i, e)| format!(".f{} = {}", i, emit_operand(e)))
                .collect();
            // A generic tuple (`Tuple<A,B>`) is named by its instance struct.
            let name = if ty.contains('<') {
                mir::inst_c_name(ty)
            } else {
                ty.clone()
            };
            format!("(struct {}){{ {} }}", name, inits.join(", "))
        }
        mir::Rvalue::Cast(o, ty) => format!("({}){}", ty, emit_operand(o)),
        mir::Rvalue::StoreField(s, f, v, deref) => {
            if *deref {
                format!("{}->{} = {}", emit_operand(s), f, emit_operand(v))
            } else {
                format!("{}.{} = {}", emit_operand(s), f, emit_operand(v))
            }
        }
        // Array literals are emitted inline in `emit_function`; this arm exists
        // only to keep the match exhaustive.
        mir::Rvalue::Array(..) => unreachable!("array literals are emitted inline"),
    }
}

fn c_binop(op: mir::MirBinOp) -> &'static str {
    use mir::MirBinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        And => "&&",
        Or => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
        Assign => "=",
    }
}

fn emit_terminator(t: &mir::Terminator) -> String {
    match t {
        mir::Terminator::Goto(n) => format!("    goto b{};\n", n),
        mir::Terminator::If(c, t, e) => {
            format!(
                "    if ({}) goto b{}; else goto b{};\n",
                emit_operand(c),
                t,
                e
            )
        }
        mir::Terminator::Return(Some(v)) => format!("    return {};\n", emit_operand(v)),
        mir::Terminator::Return(None) => "    return;\n".to_string(),
        mir::Terminator::Switch(disc, cases, default) => {
            let mut s = format!("    switch ({}) {{\n", emit_operand(disc));
            for (label, target) in cases {
                s.push_str(&format!("        case {}: goto b{};\n", label, target));
            }
            s.push_str(&format!("        default: goto b{};\n", default));
            s.push_str("    }\n");
            s
        }
        mir::Terminator::Unreachable => "    ;\n".to_string(),
    }
}

/// Scan a type string for generic instances: `Option<..>`, `Result<..>` and
/// `Tuple<..>` values are structs; `Map<K,V>` is a hash map to generate.
fn scan_generics(
    ty: &str,
    insts: &mut std::collections::BTreeSet<String>,
    maps: &mut std::collections::BTreeSet<(String, String)>,
) {
    // A pointer to an instance (`Map<str,Expr>*`, a `&HashMap` parameter) names the same
    // instance: scan the pointee.
    let (base, args) = match mir::split_generic(ty.trim_end_matches('*')) {
        Some(p) => p,
        None => return,
    };
    match base {
        "Option" | "Result" | "Tuple" => {
            insts.insert(ty.to_string());
        }
        "Map" | "Sorted" if args.len() == 2 => {
            maps.insert((args[0].to_string(), args[1].to_string()));
        }
        "Set" | "SortedSet" if args.len() == 1 => {
            maps.insert((args[0].to_string(), "bool".to_string()));
        }
        _ => {}
    }
    for a in args {
        scan_generics(a, insts, maps);
    }
}

/// The struct definitions of the generic instances in the program, and the hash maps
/// it uses (as key/value pairs). A map's `get` returns `Option<V>`, so that instance is
/// included. Instances the lowering already defined are skipped.
fn instantiations(
    prog: &mir::MirProgram,
) -> (
    Vec<mir::MirStructDef>,
    std::collections::BTreeSet<(String, String)>,
) {
    let mut types: Vec<String> = Vec::new();
    for s in &prog.structs {
        for (_, t) in &s.fields {
            types.push(t.clone());
        }
    }
    for f in &prog.functions {
        if let Some(r) = &f.ret {
            types.push(r.clone());
        }
        for (_, t) in &f.locals {
            if let Some(t) = t {
                types.push(t.clone());
            }
        }
        for t in f.temp_types.values() {
            types.push(t.clone());
        }
    }
    let mut insts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut maps: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for t in &types {
        scan_generics(t, &mut insts, &mut maps);
    }
    let mut values: Vec<String> = Vec::new();
    for (_, v) in &maps {
        values.push(format!("Option<{v}>"));
    }
    for v in &values {
        scan_generics(v, &mut insts, &mut maps);
    }
    let existing: std::collections::HashSet<String> =
        prog.structs.iter().map(|s| s.name.clone()).collect();
    let mut defs = Vec::new();
    for inst in &insts {
        let name = mir::inst_c_name(inst);
        if existing.contains(&name) {
            continue;
        }
        let (base, args) = match mir::split_generic(inst) {
            Some(p) => p,
            None => continue,
        };
        let mut fields: Vec<(String, String)> = Vec::new();
        match base {
            "Option" => {
                fields.push(("tag".to_string(), "long long".to_string()));
                fields.push(("f0".to_string(), args[0].to_string()));
            }
            "Result" => {
                fields.push(("tag".to_string(), "long long".to_string()));
                fields.push(("f0".to_string(), args[0].to_string()));
                fields.push(("e0".to_string(), args[1].to_string()));
            }
            _ => {
                for (i, a) in args.iter().enumerate() {
                    fields.push((format!("f{i}"), a.to_string()));
                }
            }
        }
        defs.push(mir::MirStructDef { name, fields });
    }
    (defs, maps)
}

/// The typedef of the hash map `Map<k, v>`: parallel key and value arrays in insertion order.
fn map_typedef(k: &str, v: &str, struct_names: &std::collections::HashSet<&str>) -> String {
    let name = mir::map_c_name(k, v);
    format!(
        "typedef struct {name} {{ {kc}* keys; {vc}* vals; long long len; long long cap; }} {name};\n",
        kc = c_type(k, struct_names),
        vc = c_type(v, struct_names),
    )
}

/// The struct of a key type (a generic instance is named by its instance struct).
fn key_struct<'a>(ty: &str, structs: &'a [mir::MirStructDef]) -> Option<&'a mir::MirStructDef> {
    let name = if ty.contains('<') {
        mir::inst_c_name(ty)
    } else {
        ty.to_string()
    };
    structs.iter().find(|s| s.name == name)
}

/// `a < b` for keys of type `ty`: strings compare with `strcmp`, a struct lexicographically
/// over its fields (in declaration order), anything else with `<`.
fn key_less(ty: &str, a: &str, b: &str, structs: &[mir::MirStructDef]) -> String {
    if ty == "char*" || ty == "str" {
        return format!("(strcmp({a}, {b}) < 0)");
    }
    if let Some(sd) = key_struct(ty, structs) {
        let mut expr = "0".to_string();
        for (f, ft) in sd.fields.iter().rev() {
            let fa = format!("{a}.{f}");
            let fb = format!("{b}.{f}");
            let lt = key_less(ft, &fa, &fb, structs);
            let eq = key_equal(ft, &fa, &fb, structs);
            expr = format!("({lt} || ({eq} && {expr}))");
        }
        return expr;
    }
    format!("({a} < {b})")
}

/// `a == b` for keys of type `ty` (see `key_less`).
fn key_equal(ty: &str, a: &str, b: &str, structs: &[mir::MirStructDef]) -> String {
    if ty == "char*" || ty == "str" {
        return format!("(strcmp({a}, {b}) == 0)");
    }
    if let Some(sd) = key_struct(ty, structs) {
        let mut parts: Vec<String> = Vec::new();
        for (f, ft) in &sd.fields {
            parts.push(key_equal(
                ft,
                &format!("{a}.{f}"),
                &format!("{b}.{f}"),
                structs,
            ));
        }
        if parts.is_empty() {
            return "1".to_string();
        }
        return format!("({})", parts.join(" && "));
    }
    format!("({a} == {b})")
}

/// The helpers of the hash map `Map<k, v>`. Keys are compared with `strcmp` for strings,
/// `memcmp` for struct keys, and `==` otherwise; iteration is in insertion order.
fn map_helpers(
    k: &str,
    v: &str,
    struct_names: &std::collections::HashSet<&str>,
    structs: &[mir::MirStructDef],
) -> String {
    let name = mir::map_c_name(k, v);
    let kc = c_type(k, struct_names);
    let vc = c_type(v, struct_names);
    let opt = c_type(&format!("Option<{v}>"), struct_names);
    let eq = if kc == "char*" {
        "strcmp(a, b) == 0".to_string()
    } else if kc.starts_with("struct ") {
        "memcmp(&a, &b, sizeof(a)) == 0".to_string()
    } else {
        "a == b".to_string()
    };
    let new = mir::map_fn_name(k, v, "new");
    let find = mir::map_fn_name(k, v, "find");
    let insert = mir::map_fn_name(k, v, "insert");
    let get = mir::map_fn_name(k, v, "get");
    let get_ptr = mir::map_fn_name(k, v, "get_ptr");
    let contains = mir::map_fn_name(k, v, "contains_key");
    let len = mir::map_fn_name(k, v, "len");
    let is_empty = mir::map_fn_name(k, v, "is_empty");
    let mut s = String::new();
    s.push_str(&format!(
        "static {name} {new}(void) {{ {name} m; m.keys = NULL; m.vals = NULL; m.len = 0; m.cap = 0; return m; }}\n"
    ));
    s.push_str(&format!("static long long {find}({name}* m, {kc} k) {{\n"));
    s.push_str("    for (long long i = 0; i < m->len; i++) {\n");
    s.push_str(&format!("        {kc} a = m->keys[i];\n"));
    s.push_str(&format!("        {kc} b = k;\n"));
    s.push_str(&format!("        if ({eq}) return i;\n"));
    s.push_str("    }\n    return -1;\n}\n");
    // `insert` returns the value it replaced, as an `Option<V>` (as Rust's does).
    s.push_str(&format!(
        "static {opt} {insert}({name}* m, {kc} k, {vc} v) {{\n"
    ));
    s.push_str(&format!("    {opt} r;\n    memset(&r, 0, sizeof(r));\n"));
    s.push_str(&format!("    long long i = {find}(m, k);\n"));
    s.push_str("    if (i >= 0) { r.tag = 0; r.f0 = m->vals[i]; m->vals[i] = v; return r; }\n");
    s.push_str("    r.tag = 1;\n");
    s.push_str("    if (m->len == m->cap) {\n");
    s.push_str("        long long nc = m->cap ? m->cap * 2 : 4;\n");
    s.push_str(&format!(
        "        m->keys = ({kc}*)realloc(m->keys, (size_t)nc * sizeof({kc}));\n"
    ));
    s.push_str(&format!(
        "        m->vals = ({vc}*)realloc(m->vals, (size_t)nc * sizeof({vc}));\n"
    ));
    s.push_str("        m->cap = nc;\n    }\n");
    s.push_str(
        "    m->keys[m->len] = k;\n    m->vals[m->len] = v;\n    m->len++;\n    return r;\n}\n",
    );
    s.push_str(&format!("static {opt} {get}({name}* m, {kc} k) {{\n"));
    s.push_str(&format!("    {opt} r;\n    memset(&r, 0, sizeof(r));\n"));
    s.push_str(&format!("    long long i = {find}(m, k);\n"));
    s.push_str(
        "    r.tag = (i < 0) ? 1 : 0;\n    if (i >= 0) r.f0 = m->vals[i];\n    return r;\n}\n",
    );
    // `get_ptr`: a pointer to the value of `k`, or NULL (the checker's `Option<&V>` payload).
    s.push_str(&format!(
        "static {vc}* {get_ptr}({name}* m, {kc} k) {{
"
    ));
    s.push_str(&format!(
        "    long long i = {find}(m, k);
"
    ));
    s.push_str(
        "    return i < 0 ? NULL : &m->vals[i];
}
",
    );
    s.push_str(&format!(
        "static long long {contains}({name}* m, {kc} k) {{ return {find}(m, k) >= 0; }}\n"
    ));
    // `remove(k)`: the removed value as an `Option<V>`; the last entry fills the freed slot.
    let remove = mir::map_fn_name(k, v, "remove");
    s.push_str(&format!("static {opt} {remove}({name}* m, {kc} k) {{\n"));
    s.push_str(&format!("    {opt} r;\n    memset(&r, 0, sizeof(r));\n"));
    s.push_str(&format!("    long long i = {find}(m, k);\n"));
    s.push_str("    r.tag = (i < 0) ? 1 : 0;\n");
    s.push_str("    if (i >= 0) {\n        r.f0 = m->vals[i];\n        m->len--;\n");
    s.push_str(
        "        m->keys[i] = m->keys[m->len];\n        m->vals[i] = m->vals[m->len];\n    }\n",
    );
    s.push_str("    return r;\n}\n");
    // `entry(k).or_insert(v)`: a pointer to the value of `k`, inserting `v` first if absent.
    let entry = mir::map_fn_name(k, v, "entry_or_insert");
    s.push_str(&format!(
        "static {vc}* {entry}({name}* m, {kc} k, {vc} v) {{\n    long long i = {find}(m, k);\n    if (i < 0) {{ {insert}(m, k, v); i = m->len - 1; }}\n    return &m->vals[i];\n}}\n"
    ));
    s.push_str(&format!(
        "static long long {len}({name}* m) {{ return m->len; }}\n"
    ));
    s.push_str(&format!(
        "static long long {is_empty}({name}* m) {{ return m->len == 0; }}\n"
    ));
    // `clear()`: the table keeps its storage and forgets its entries.
    let clear = mir::map_fn_name(k, v, "clear");
    s.push_str(&format!(
        "static long long {clear}({name}* m) {{ m->len = 0; return 0; }}\n"
    ));
    // `rank(m, i)`: the slot of the i-th smallest key (sorted maps iterate through it).
    let less = key_less(k, "a", "b", structs);
    let rank = mir::map_fn_name(k, v, "rank");
    s.push_str(&format!(
        "static long long {rank}({name}* m, long long i) {{\n"
    ));
    s.push_str("    for (long long j = 0; j < m->len; j++) {\n");
    s.push_str("        long long below = 0;\n");
    s.push_str("        for (long long t = 0; t < m->len; t++) {\n");
    s.push_str(&format!("            {kc} a = m->keys[t];\n"));
    s.push_str(&format!("            {kc} b = m->keys[j];\n"));
    s.push_str(&format!("            if ({less}) below++;\n"));
    s.push_str("        }\n");
    s.push_str("        if (below == i) return j;\n    }\n    return -1;\n}\n\n");
    s
}
