#include "glue.h"

#include <sofia-sip/nua.h>
#include <sofia-sip/su_glib.h>
#include <sofia-sip/sip_status.h>
#include <sofia-sip/sdp.h>
#include <sofia-sip/auth_digest.h>
#include <sofia-sip/tport_tag.h>
#include <stdio.h>

#include <glib.h>
#include <stdlib.h>
#include <string.h>
#include <netdb.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <unistd.h>

/* ── Internal context ─────────────────────────────────────────────────────── */

struct SofiaCtx {
    su_root_t        *root;
    nua_t            *nua;
    nua_handle_t     *reg_nh;
    nua_handle_t     *call_nh;

    sofia_event_cb_t  cb;
    void             *userdata;

    char             *user;
    char             *password;
    char             *server;
    int               sip_port;   /* registrar port stored for explicit re-REGISTER */
    char             *proxy;           /* outbound proxy SIP URI, NULL if none */
    int               transport;       /* TRANSPORT_UDP / TCP / TLS */
    char              tls_cert_dir[512]; /* temp dir holding cafile.pem, empty if unused */
    char             *auth_str;
    char             *call_to;    /* To URI of the current outgoing call */
    char              local_ip[INET_ADDRSTRLEN];

    int               local_rtp_port;
    char              remote_rtp_ip[64];
    int               remote_rtp_port;
    int               remote_rtp_payload; /* selected RTP payload type (0=PCMU, 8=PCMA) */

    gboolean          reg_auth_tried;   /* true after one auth attempt in the current REGISTER cycle */
    gboolean          call_auth_tried;  /* true after first digest attempt */
    gboolean          call_established; /* true after the first 200 OK for INVITE */
    gboolean          call_is_incoming; /* true while call_nh is a ringing inbound call awaiting answer */
    gboolean          call_on_hold;
    gboolean          shutting_down;
    gboolean          shutdown_done;

    /* On every startup, before doing the normal REGISTER, we first send
       REGISTER Contact:* Expires:0 to Asterisk.  This removes ALL bindings
       from previous sessions that were never properly unregistered (because
       std::process::exit() bypasses Rust/GObject destructors).  Without
       this, Asterisk accumulates stale contacts and routes incoming calls
       to dead old ports.  cleanup_registrar stores the URI for the deferred
       real nua_register() call after the cleanup completes. */
    gboolean          cleanup_pending;
    char             *cleanup_registrar;


    /* Consultation call fields */
    nua_handle_t     *consult_nh;
    char             *consult_to;          /* SIP URI of consultation party */
    int               consult_local_rtp_port;
    char              consult_remote_ip[64];
    int               consult_remote_port;
    int               consult_remote_payload;
    gboolean          consult_established;
    gboolean          consult_auth_tried;
    gboolean          consult_ended; /* CONSULT_ENDED already fired; suppress duplicate */

    /* Dialog identifiers captured from the consultation 200 OK, used to build
       the Replaces header in the attended-transfer Refer-To URI. */
    char             *consult_call_id;
    char             *consult_from_tag; /* our tag (From) in the consult dialog */
    char             *consult_to_tag;   /* 886's tag (To) in the consult dialog */
};

/* ── Transport helpers ────────────────────────────────────────────────────── */

static const char *sip_scheme(const SofiaCtx *ctx) {
    return ctx->transport == TRANSPORT_TLS ? "sips" : "sip";
}

/* Returns ";transport=tcp" for TCP, "" for UDP and TLS (TLS uses sips: scheme). */
static const char *transport_param(const SofiaCtx *ctx) {
    return ctx->transport == TRANSPORT_TCP ? ";transport=tcp" : "";
}

/* Build the Contact header for INVITE/re-INVITE.  Sofia 1.13 omits an
   auto-generated Contact in some cases, so every INVITE we send (initial,
   auth-retry, hold re-INVITE, consultation) sets it explicitly. */
static void build_contact(const SofiaCtx *ctx, char *buf, size_t len) {
    if (ctx->transport == TRANSPORT_TLS)
        snprintf(buf, len, "<sips:%s@%s>", ctx->user, ctx->local_ip);
    else if (ctx->transport == TRANSPORT_TCP)
        snprintf(buf, len, "<sip:%s@%s;transport=tcp>", ctx->user, ctx->local_ip);
    else
        snprintf(buf, len, "<sip:%s@%s>", ctx->user, ctx->local_ip);
}

/* ── Local-interface selection ────────────────────────────────────────────── */

/* Connect a throw-away UDP socket to the destination so the kernel picks the
   right source address, then read it back with getsockname(). */
static void get_local_ip_for(const char *host, int port,
                              char *buf, size_t buflen)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family   = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;

    char port_str[16];
    snprintf(port_str, sizeof(port_str), "%d", port);

    if (getaddrinfo(host, port_str, &hints, &res) != 0 || !res)
        return;

    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) { freeaddrinfo(res); return; }

    if (connect(sock, res->ai_addr, (socklen_t)res->ai_addrlen) == 0) {
        struct sockaddr_in local;
        socklen_t local_len = sizeof(local);
        if (getsockname(sock, (struct sockaddr *)&local, &local_len) == 0)
            inet_ntop(AF_INET, &local.sin_addr, buf, (socklen_t)buflen);
    }
    close(sock);
    freeaddrinfo(res);
}

/* Bind a UDP socket to port 0 and read back the ephemeral port assigned. */
static int get_free_udp_port(void) {
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) return 10000 + rand() % 10000;
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    bind(sock, (struct sockaddr *)&addr, sizeof(addr));
    socklen_t len = sizeof(addr);
    getsockname(sock, (struct sockaddr *)&addr, &len);
    int port = ntohs(addr.sin_port);
    close(sock);
    return port > 0 ? port : 10000 + rand() % 10000;
}

/* Bind a TCP socket to port 0 and read back the ephemeral port assigned.
   Used to obtain a free port for TCP/TLS NUA listeners so that the port
   appears correctly in the Contact header (port 0 in the NUA URL causes
   sofia-sip 1.13+ to omit the Contact header entirely from INVITE). */
static int get_free_tcp_port(void) {
    int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) return 5060;
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    bind(sock, (struct sockaddr *)&addr, sizeof(addr));
    socklen_t len = sizeof(addr);
    getsockname(sock, (struct sockaddr *)&addr, &len);
    int port = ntohs(addr.sin_port);
    close(sock);
    return port > 0 ? port : 5080;
}

/* Extract the first audio stream's connection address, port, and selected
   payload type from the SDP body of a SIP message, writing into the
   provided output buffers. This can be used for both the primary and
   consultation calls without touching ctx fields directly. */
static void extract_rtp_into(SofiaCtx *ctx, sip_t const *sip,
                              char *ip_out, size_t ip_len,
                              int *port_out, int *payload_out)
{
    (void)ctx;
    if (!sip || !sip->sip_payload || !sip->sip_payload->pl_data)
        return;

    su_home_t home[1];
    su_home_init(home);

    sdp_parser_t *p = sdp_parse(home,
                                 sip->sip_payload->pl_data,
                                 (int)sip->sip_payload->pl_len, 0);
    sdp_session_t const *sdp = p ? sdp_session(p) : NULL;
    if (sdp) {
        const char *c_addr = NULL;
        if (sdp->sdp_connection && sdp->sdp_connection->c_address)
            c_addr = sdp->sdp_connection->c_address;

        for (sdp_media_t const *m = sdp->sdp_media; m; m = m->m_next) {
            if (m->m_type != sdp_media_audio) continue;
            const char *m_addr = c_addr;
            if (m->m_connections && m->m_connections->c_address)
                m_addr = m->m_connections->c_address;
            if (m_addr && ip_len > 0) {
                strncpy(ip_out, m_addr, ip_len - 1);
                ip_out[ip_len - 1] = '\0'; /* strncpy won't terminate on truncation */
            }
            *port_out = (int)m->m_port;

            /* Pick the first non-telephone-event payload type as the codec. */
            *payload_out = 0; /* default PCMU */
            for (sdp_rtpmap_t const *rm = m->m_rtpmaps; rm; rm = rm->rm_next) {
                if (rm->rm_pt != 101) {
                    *payload_out = (int)rm->rm_pt;
                    break;
                }
            }
            break;
        }
    }

    su_home_deinit(home);
}

/* Ensure ctx->local_ip holds a usable source address for media.  The address
   is picked once at context creation, but that can fail if the network is down
   at startup (DNS failure / no route), leaving "0.0.0.0", which would advertise
   an unroutable address in SDP and produce a connected call with no audio.
   Re-resolve here against the registrar so a later (network-up) call recovers,
   and warn loudly if it still cannot be determined. */
static void ensure_local_ip(SofiaCtx *ctx) {
    if (ctx->local_ip[0] && strcmp(ctx->local_ip, "0.0.0.0") != 0)
        return;
    if (ctx->server && *ctx->server)
        get_local_ip_for(ctx->server, ctx->sip_port > 0 ? ctx->sip_port : 5060,
                         ctx->local_ip, sizeof(ctx->local_ip));
    if (!ctx->local_ip[0] || strcmp(ctx->local_ip, "0.0.0.0") == 0)
        fprintf(stderr, "[tmwphone] WARNING: could not determine a local IP for "
                        "media (network down?); call audio will not work\n");
}

/* Build an SDP offer/answer for ctx->local_ip:port.
   direction is "sendrecv", "sendonly", or "recvonly".
   port is the local RTP port to advertise. */
static void build_audio_sdp(SofiaCtx *ctx, char *buf, size_t len,
                             const char *direction, int port) {
    ensure_local_ip(ctx);
    snprintf(buf, len,
        "v=0\r\n"
        "o=- 0 0 IN IP4 %s\r\n"
        "s=-\r\n"
        "c=IN IP4 %s\r\n"
        "t=0 0\r\n"
        "m=audio %d RTP/AVP 0 8 101\r\n"
        "a=rtpmap:0 PCMU/8000\r\n"
        "a=rtpmap:8 PCMA/8000\r\n"
        "a=rtpmap:101 telephone-event/8000\r\n"
        "a=fmtp:101 0-15\r\n"
        "a=ptime:20\r\n"
        "a=%s\r\n",
        ctx->local_ip, ctx->local_ip, port, direction);
}

/* ── Auth helpers ─────────────────────────────────────────────────────────── */

/* Compute MD5(str) and write the 32-char lowercase hex result into out[33]. */
static void md5hex(const char *str, char out[33]) {
    char *h = g_compute_checksum_for_string(G_CHECKSUM_MD5, str, -1);
    memcpy(out, h, 32);
    out[32] = '\0';
    g_free(h);
}

/* Compute Digest response = MD5(HA1:nonce:HA2) or MD5(HA1:nonce:nc:cnonce:qop:HA2).
   ha1_hex is MD5(user:realm:password), ha2_hex is MD5(method:uri). */
static void digest_response(const char *ha1_hex, const char *nonce,
                             int use_qop, const char *nc, const char *cnonce,
                             const char *ha2_hex, char out[33]) {
    char buf[512];
    if (use_qop)
        snprintf(buf, sizeof(buf), "%s:%s:%s:%s:auth:%s", ha1_hex, nonce, nc, cnonce, ha2_hex);
    else
        snprintf(buf, sizeof(buf), "%s:%s:%s", ha1_hex, nonce, ha2_hex);
    md5hex(buf, out);
}

static void build_auth(SofiaCtx *ctx, const char *realm) {
    char buf[1024];
    snprintf(buf, sizeof(buf), "Digest:\"%s\":%s:%s", realm, ctx->user, ctx->password);
    free(ctx->auth_str);
    ctx->auth_str = strdup(buf);
}

static char *extract_realm(msg_auth_t const *auth) {
    if (!auth || !auth->au_params) return NULL;
    for (int i = 0; auth->au_params[i]; i++) {
        const char *p = auth->au_params[i];
        if (strncmp(p, "realm=", 6) != 0) continue;
        p += 6;
        if (*p == '"') p++;
        char *realm = strdup(p);
        char *end = strchr(realm, '"');
        if (end) *end = '\0';
        return realm;
    }
    return NULL;
}

/* Generic version: extract any named parameter from the auth header params list.
   Returns a heap-allocated string (caller must free) or NULL if not found. */
static char *extract_param(msg_auth_t const *auth, const char *name) {
    if (!auth || !auth->au_params) return NULL;
    size_t nlen = strlen(name);
    for (int i = 0; auth->au_params[i]; i++) {
        const char *p = auth->au_params[i];
        if (strncmp(p, name, nlen) != 0 || p[nlen] != '=') continue;
        p += nlen + 1;
        if (*p == '"') p++;
        char *val = strdup(p);
        char *end = strchr(val, '"');
        if (end) *end = '\0';
        return val;
    }
    return NULL;
}

/* ── Digest auth (manual, bypasses nua_authenticate which is broken in
      libsofia-sip-ua 1.12.11) ─────────────────────────────────────────── */

/* Compute digest response and re-send INVITE with Authorization header.
   Called from nua_r_invite when status is 401 or 407. */
static void invite_with_digest(SofiaCtx *ctx, sip_t const *sip, int status)
{
    if (!sip) {
        ctx->cb(SOFIA_EV_CALL_FAILED, status, "No SIP message in auth challenge", NULL, ctx->userdata);
        return;
    }

    /* Only attempt auth once per call to prevent infinite retry loops. */
    if (ctx->call_auth_tried) {
        ctx->cb(SOFIA_EV_CALL_FAILED, status, "Authentication failed", NULL, ctx->userdata);
        if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        return;
    }
    ctx->call_auth_tried = TRUE;

    msg_auth_t const *ch = (status == 401)
    ? sip->sip_www_authenticate
    : sip->sip_proxy_authenticate;
    if (!ch) {
        ctx->cb(SOFIA_EV_CALL_FAILED, status, "Missing auth challenge header", NULL, ctx->userdata);
        if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        return;
    }

    /* Use the same manual param extraction that already works for REGISTER auth,
     *      rather than auth_digest_challenge_get which has more failure modes. */
    char *realm_str  = extract_param(ch, "realm");
    char *nonce_str  = extract_param(ch, "nonce");
    char *qop_str    = extract_param(ch, "qop");
    char *opaque_str = extract_param(ch, "opaque");

    const char *realm = realm_str ? realm_str : ctx->server;
    const char *nonce = nonce_str ? nonce_str : "";

    build_auth(ctx, realm);

    /* Use the To URI saved at call initiation time. */
    const char *to_uri = ctx->call_to ? ctx->call_to : "";
    char to_hdr[520];
    snprintf(to_hdr, sizeof(to_hdr), "<%s>", to_uri);

    /* Compute Digest credentials using GLib MD5 (no dependency on sofia's
     *      auth_digest_ha1 / auth_digest_response which can silently misbehave).
     * cnonce is freshly randomised per request (with qop=auth a fixed cnonce
     * would defeat the client-side replay protection); nc stays 00000001 since
     * each fresh cnonce is used exactly once. */
    char cnonce_buf[17];
    snprintf(cnonce_buf, sizeof(cnonce_buf), "%08x%08x", g_random_int(), g_random_int());
    const char *cnonce = cnonce_buf;
    const char *nc     = "00000001";
    int use_qop = qop_str && strcmp(qop_str, "auth") == 0;

    char ha1_input[512], ha1[33];
    snprintf(ha1_input, sizeof(ha1_input), "%s:%s:%s", ctx->user, realm, ctx->password);
    md5hex(ha1_input, ha1);

    char ha2_input[512], ha2[33];
    snprintf(ha2_input, sizeof(ha2_input), "INVITE:%s", to_uri);
    md5hex(ha2_input, ha2);

    char hexresp[33];
    digest_response(ha1, nonce, use_qop, nc, cnonce, ha2, hexresp);

    /* Verbose auth tracing is gated behind an env var so credentials-adjacent
       material (realm/nonce/response) never lands in stderr by default. */
    if (getenv("TMWPHONE_DEBUG_AUTH"))
        fprintf(stderr, "[tmwphone] INVITE auth: user=%s realm=%s uri=%s\n",
                ctx->user, realm, to_uri);
    if (ctx->password && ctx->password[0] == '\0')
        fprintf(stderr, "[tmwphone] WARNING: password is empty — did you save the account before connecting?\n");

    char auth_hdr[2048];
    int n = snprintf(auth_hdr, sizeof(auth_hdr),
                     "Digest username=\"%s\", realm=\"%s\", nonce=\"%s\","
                     " uri=\"%s\", response=\"%s\", algorithm=MD5",
                     ctx->user, realm, nonce, to_uri, hexresp);
    if (use_qop && n > 0 && n < (int)sizeof(auth_hdr))
        n += snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
                      ", qop=auth, nc=%s, cnonce=\"%s\"", nc, cnonce);
        if (opaque_str && n > 0 && n < (int)sizeof(auth_hdr))
            snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
                     ", opaque=\"%s\"", opaque_str);

            /* Preserve the Call-ID and From tag from the 401 response before destroying
             *      the old handle.  Calling nua_invite on the same handle (after the 401
             *      terminated its transaction) is silently dropped by sofia's NUA state
             *      machine; we need a fresh handle.  But RFC 3261 §22.1 requires the retry
             *      to carry the same Call-ID and From tag, otherwise Asterisk treats the
             *      retry as a brand-new call and issues a fresh 401. */
            char preserved_call_id[256] = {0};
        char from_hdr[320];
        if (sip && sip->sip_call_id && sip->sip_call_id->i_id)
            snprintf(preserved_call_id, sizeof(preserved_call_id),
                     "%s", sip->sip_call_id->i_id);
            if (sip && sip->sip_from) {
                sip_from_t *f = sip->sip_from;
                const char *u = f->a_url->url_user ? f->a_url->url_user : ctx->user;
                const char *h = f->a_url->url_host ? f->a_url->url_host : ctx->server;
                if (f->a_tag)
                    snprintf(from_hdr, sizeof(from_hdr),
                             "<%s:%s@%s>;tag=%s", sip_scheme(ctx), u, h, f->a_tag);
                    else
                        snprintf(from_hdr, sizeof(from_hdr),
                                 "<%s:%s@%s>", sip_scheme(ctx), u, h);
            } else {
                snprintf(from_hdr, sizeof(from_hdr),
                         "<%s:%s@%s>", sip_scheme(ctx), ctx->user, ctx->server);
            }

            char contact[512];
            build_contact(ctx, contact, sizeof(contact));

            if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
            if (preserved_call_id[0])
                ctx->call_nh = nua_handle(ctx->nua, NULL,
                                          SIPTAG_FROM_STR(from_hdr),
                                          SIPTAG_TO_STR(to_hdr),
                                          SIPTAG_CALL_ID_STR(preserved_call_id),
                                          TAG_END());
                else
                    ctx->call_nh = nua_handle(ctx->nua, NULL,
                                              SIPTAG_FROM_STR(from_hdr),
                                              SIPTAG_TO_STR(to_hdr),
                                              TAG_END());

                    free(realm_str); free(nonce_str); free(qop_str); free(opaque_str);

                ctx->local_rtp_port = get_free_udp_port();
                char sdp[512];
                build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->local_rtp_port);

                /* SIPTAG_CONTACT_STR hier in beide Zweige eingefügt: */
                if (status == 401)
                    nua_invite(ctx->call_nh,
                               SIPTAG_CONTACT_STR(contact), // <-- Eingefügt
                               SIPTAG_AUTHORIZATION_STR(auth_hdr),
                               SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                               SIPTAG_PAYLOAD_STR(sdp),
                               TAG_END());
                    else
                        nua_invite(ctx->call_nh,
                                   SIPTAG_CONTACT_STR(contact), // <-- Eingefügt
                                   SIPTAG_PROXY_AUTHORIZATION_STR(auth_hdr),
                                   SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                                   SIPTAG_PAYLOAD_STR(sdp),
                                   TAG_END());
}

/* Compute digest response and re-send consultation INVITE with Authorization header.
   Parallel to invite_with_digest but operates on consult_nh and consult_to. */
static void consult_with_digest(SofiaCtx *ctx, sip_t const *sip, int status)
{
    if (!sip) {
        ctx->cb(SOFIA_EV_CONSULT_ENDED, status, "No SIP message in auth challenge", NULL, ctx->userdata);
        return;
    }

    /* Only attempt auth once per consultation call. */
    if (ctx->consult_auth_tried) {
        ctx->cb(SOFIA_EV_CONSULT_ENDED, status, "Authentication failed", NULL, ctx->userdata);
        if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
        return;
    }
    ctx->consult_auth_tried = TRUE;

    msg_auth_t const *ch = (status == 401)
        ? sip->sip_www_authenticate
        : sip->sip_proxy_authenticate;
    if (!ch) {
        ctx->cb(SOFIA_EV_CONSULT_ENDED, status, "Missing auth challenge header", NULL, ctx->userdata);
        if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
        return;
    }

    char *realm_str  = extract_param(ch, "realm");
    char *nonce_str  = extract_param(ch, "nonce");
    char *qop_str    = extract_param(ch, "qop");
    char *opaque_str = extract_param(ch, "opaque");

    const char *realm = realm_str ? realm_str : ctx->server;
    const char *nonce = nonce_str ? nonce_str : "";

    const char *to_uri = ctx->consult_to ? ctx->consult_to : "";
    char to_hdr[520];
    snprintf(to_hdr, sizeof(to_hdr), "<%s>", to_uri);

    /* Fresh per-request cnonce; see invite_with_digest. */
    char cnonce_buf[17];
    snprintf(cnonce_buf, sizeof(cnonce_buf), "%08x%08x", g_random_int(), g_random_int());
    const char *cnonce = cnonce_buf;
    const char *nc     = "00000001";
    int use_qop = qop_str && strcmp(qop_str, "auth") == 0;

    char ha1_input[512], ha1[33];
    snprintf(ha1_input, sizeof(ha1_input), "%s:%s:%s", ctx->user, realm, ctx->password);
    md5hex(ha1_input, ha1);

    char ha2_input[512], ha2[33];
    snprintf(ha2_input, sizeof(ha2_input), "INVITE:%s", to_uri);
    md5hex(ha2_input, ha2);

    char hexresp[33];
    digest_response(ha1, nonce, use_qop, nc, cnonce, ha2, hexresp);

    char auth_hdr[2048];
    int n = snprintf(auth_hdr, sizeof(auth_hdr),
        "Digest username=\"%s\", realm=\"%s\", nonce=\"%s\","
        " uri=\"%s\", response=\"%s\", algorithm=MD5",
        ctx->user, realm, nonce, to_uri, hexresp);
    if (use_qop && n > 0 && n < (int)sizeof(auth_hdr))
        n += snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
            ", qop=auth, nc=%s, cnonce=\"%s\"", nc, cnonce);
    if (opaque_str && n > 0 && n < (int)sizeof(auth_hdr))
        snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
            ", opaque=\"%s\"", opaque_str);

    char cons_call_id[256] = {0};
    char cons_from_hdr[320];
    if (sip && sip->sip_call_id && sip->sip_call_id->i_id)
        snprintf(cons_call_id, sizeof(cons_call_id),
                 "%s", sip->sip_call_id->i_id);
    if (sip && sip->sip_from) {
        sip_from_t *f = sip->sip_from;
        const char *u = f->a_url->url_user ? f->a_url->url_user : ctx->user;
        const char *h = f->a_url->url_host ? f->a_url->url_host : ctx->server;
        if (f->a_tag)
            snprintf(cons_from_hdr, sizeof(cons_from_hdr),
                     "<%s:%s@%s>;tag=%s", sip_scheme(ctx), u, h, f->a_tag);
        else
            snprintf(cons_from_hdr, sizeof(cons_from_hdr),
                     "<%s:%s@%s>", sip_scheme(ctx), u, h);
    } else {
        snprintf(cons_from_hdr, sizeof(cons_from_hdr),
                 "<%s:%s@%s>", sip_scheme(ctx), ctx->user, ctx->server);
    }

    if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
    if (cons_call_id[0])
        ctx->consult_nh = nua_handle(ctx->nua, NULL,
                                      SIPTAG_FROM_STR(cons_from_hdr),
                                      SIPTAG_TO_STR(to_hdr),
                                      SIPTAG_CALL_ID_STR(cons_call_id),
                                      TAG_END());
    else
        ctx->consult_nh = nua_handle(ctx->nua, NULL,
                                      SIPTAG_FROM_STR(cons_from_hdr),
                                      SIPTAG_TO_STR(to_hdr),
                                      TAG_END());

    free(realm_str); free(nonce_str); free(qop_str); free(opaque_str);

    ctx->consult_local_rtp_port = get_free_udp_port();
    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->consult_local_rtp_port);

    char contact[512];
    build_contact(ctx, contact, sizeof(contact));

    if (status == 401)
        nua_invite(ctx->consult_nh,
                   SIPTAG_CONTACT_STR(contact),
                   SIPTAG_AUTHORIZATION_STR(auth_hdr),
                   SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                   SIPTAG_PAYLOAD_STR(sdp),
                   TAG_END());
    else
        nua_invite(ctx->consult_nh,
                   SIPTAG_CONTACT_STR(contact),
                   SIPTAG_PROXY_AUTHORIZATION_STR(auth_hdr),
                   SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                   SIPTAG_PAYLOAD_STR(sdp),
                   TAG_END());
}

/* ── NUA callback ─────────────────────────────────────────────────────────── */

static void nua_cb(nua_event_t event, int status, char const *phrase,
                   nua_t *nua, nua_magic_t *magic,
                   nua_handle_t *nh, nua_hmagic_t *hmagic,
                   sip_t const *sip, tagi_t tags[])
{
    (void)nua; (void)hmagic; (void)tags;

    SofiaCtx *ctx = (SofiaCtx *)magic;

    switch (event) {

    case nua_r_shutdown:
        ctx->shutdown_done = TRUE;
        return;

    case nua_r_register:
        if (status == 200) {
            ctx->reg_auth_tried = FALSE; /* reset for the next refresh cycle */
            ctx->cb(SOFIA_EV_REGISTER_OK, status, phrase, NULL, ctx->userdata);
        } else if ((status == 401 || status == 407) && ctx->user && ctx->password
                   && !ctx->reg_auth_tried) {
            /* Authenticate exactly once per REGISTER cycle.  A second challenge
               after we already presented credentials means they were rejected
               (wrong password / stale nonce) — re-authenticating would loop
               REGISTER↔401 indefinitely, so fall through to REGISTER_FAIL. */
            ctx->reg_auth_tried = TRUE;
            msg_auth_t const *challenge = (status == 401)
                ? sip->sip_www_authenticate
                : sip->sip_proxy_authenticate;
            char *realm = extract_realm(challenge);
            build_auth(ctx, realm ? realm : ctx->server);
            free(realm);
            /* nua_authenticate() passes credentials at the handle level;
               sofia caches them for keepalive REGISTER refreshes on this
               handle, so no NUA-level nua_set_params(NUTAG_AUTH) is needed.
               Calling nua_set_params here fires nua_r_set_params (event=23)
               which has no functional benefit and adds unnecessary
               state-machine churn before the 200 OK arrives. */
            nua_authenticate(nh, NUTAG_AUTH(ctx->auth_str), TAG_END());
        } else if (status >= 300) {
            ctx->reg_auth_tried = FALSE;
            ctx->cb(SOFIA_EV_REGISTER_FAIL, status, phrase, NULL, ctx->userdata);
        }
        /* 1xx provisional and internal (0) statuses: wait for final response. */
        break;

    case nua_r_unregister:
        if (!ctx->cleanup_pending) break;

        if ((status == 401 || status == 407) && !ctx->reg_auth_tried) {
            /* Asterisk challenges the wildcard unregister — authenticate once. */
            ctx->reg_auth_tried = TRUE;
            msg_auth_t const *challenge = (status == 401)
                ? sip->sip_www_authenticate
                : sip->sip_proxy_authenticate;
            char *realm = extract_realm(challenge);
            build_auth(ctx, realm ? realm : ctx->server);
            free(realm);
            nua_authenticate(nh, NUTAG_AUTH(ctx->auth_str), TAG_END());
        } else {
            /* 200 OK (all contacts removed), any error (404, 403, ...), or a
               repeated challenge after we already authenticated — either way,
               proceed with the real registration.  The cleanup is best-effort;
               errors just mean there was nothing to remove, which is fine.
               Reset reg_auth_tried so the REGISTER's own first challenge is
               not mistaken for a retry. */
            ctx->cleanup_pending = FALSE;
            ctx->reg_auth_tried  = FALSE;
            nua_register(ctx->reg_nh,
                         NUTAG_REGISTRAR(ctx->cleanup_registrar),
                         TAG_END());
        }
        break;

    case nua_i_invite: {
        /* Re-INVITE on an established call (remote-initiated hold or
           codec renegotiation).  Respond with 200 OK and current SDP;
           do NOT fire SOFIA_EV_INCOMING_CALL or reset call state. */
        if (ctx->call_established && ctx->call_nh == nh) {
            extract_rtp_into(ctx, sip,
                             ctx->remote_rtp_ip, sizeof(ctx->remote_rtp_ip),
                             &ctx->remote_rtp_port, &ctx->remote_rtp_payload);
            char sdp[512];
            build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->local_rtp_port);
            nua_respond(ctx->call_nh, SIP_200_OK,
                        SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                        SIPTAG_PAYLOAD_STR(sdp),
                        TAG_END());
            break;
        }

        /* New incoming call.  If we're already busy with another call (active,
           ringing inbound, or an outbound attempt in progress), reject this one
           with 486 Busy Here.  Clobbering ctx->call_nh here would orphan the
           existing call's handle and leave its audio/UI state dangling. */
        if (ctx->call_nh && ctx->call_nh != nh) {
            nua_respond(nh, SIP_486_BUSY_HERE, TAG_END());
            break;
        }
        ctx->call_nh = nua_handle_ref(nh);

        /* Reset per-call state so stale flags from a previous call never leak
           into this new incoming dialog (e.g. call_established = TRUE would
           cause sofia_set_hold to send a re-INVITE on this handle). */
        ctx->call_established = FALSE;
        ctx->call_auth_tried  = FALSE;
        ctx->call_on_hold     = FALSE;
        ctx->call_is_incoming = TRUE;

        /* Store caller's SDP offer so sofia_answer() can include it later. */
        extract_rtp_into(ctx, sip,
                         ctx->remote_rtp_ip, sizeof(ctx->remote_rtp_ip),
                         &ctx->remote_rtp_port, &ctx->remote_rtp_payload);

        char from_buf[256] = {0};
        if (sip && sip->sip_from) {
            sip_from_t *f = sip->sip_from;
            const char *u = f->a_url->url_user ? f->a_url->url_user : "";
            const char *h = f->a_url->url_host ? f->a_url->url_host : "";
            if (f->a_display && f->a_display[0])
                snprintf(from_buf, sizeof(from_buf), "%s <%s@%s>", f->a_display, u, h);
            else
                snprintf(from_buf, sizeof(from_buf), "%s@%s", u, h);
        }

        /* Send 180 Ringing so the remote party knows we received the INVITE
           and stops retransmitting while the user decides to answer. */
        nua_respond(nh, SIP_180_RINGING, TAG_END());

        ctx->cb(SOFIA_EV_INCOMING_CALL, status, phrase,
                from_buf[0] ? from_buf : "Unknown", ctx->userdata);
        break;
    }

    case nua_r_invite:
        /* Check for consultation handle FIRST to avoid mishandling consult 200 OKs */
        if (nh == ctx->consult_nh) {
            if (status >= 200 && status < 300) {
                nua_ack(nh, TAG_END());
                if (!ctx->consult_established) {
                    ctx->consult_established = TRUE;
                    extract_rtp_into(ctx, sip,
                        ctx->consult_remote_ip, sizeof(ctx->consult_remote_ip),
                        &ctx->consult_remote_port, &ctx->consult_remote_payload);

                    /* Save dialog identifiers for attended-transfer Replaces header. */
                    free(ctx->consult_call_id);  ctx->consult_call_id  = NULL;
                    free(ctx->consult_from_tag); ctx->consult_from_tag = NULL;
                    free(ctx->consult_to_tag);   ctx->consult_to_tag   = NULL;
                    if (sip && sip->sip_call_id)
                        ctx->consult_call_id = strdup(sip->sip_call_id->i_id);
                    if (sip && sip->sip_from && sip->sip_from->a_tag)
                        ctx->consult_from_tag = strdup(sip->sip_from->a_tag);
                    if (sip && sip->sip_to && sip->sip_to->a_tag)
                        ctx->consult_to_tag = strdup(sip->sip_to->a_tag);

                    ctx->cb(SOFIA_EV_CONSULT_CONNECTED, status, phrase, NULL, ctx->userdata);
                    if (ctx->consult_local_rtp_port > 0 && ctx->consult_remote_port > 0) {
                        char aux[128];
                        snprintf(aux, sizeof(aux), "%d,%s,%d,%d",
                            ctx->consult_local_rtp_port, ctx->consult_remote_ip,
                            ctx->consult_remote_port, ctx->consult_remote_payload);
                        ctx->cb(SOFIA_EV_CONSULT_MEDIA, status, phrase, aux, ctx->userdata);
                    }
                }
                /* re-INVITE 200 OK — already ACK'd, nothing else to do. */
            } else if ((status == 401 || status == 407) && ctx->user && ctx->password) {
                consult_with_digest(ctx, sip, status);
            } else if (status >= 300) {
                if (!ctx->consult_ended)
                    ctx->cb(SOFIA_EV_CONSULT_ENDED, status, phrase, NULL, ctx->userdata);
                ctx->consult_ended = FALSE;
                ctx->consult_established = FALSE;
                if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
            }
            break;
        }
        /* Primary call handling */
        if (status >= 200 && status < 300) {
            nua_ack(nh, TAG_END());
            if (!ctx->call_established) {
                /* Initial INVITE 200 OK — start media. */
                ctx->call_established = TRUE;
                extract_rtp_into(ctx, sip,
                                 ctx->remote_rtp_ip, sizeof(ctx->remote_rtp_ip),
                                 &ctx->remote_rtp_port, &ctx->remote_rtp_payload);
                ctx->cb(SOFIA_EV_CALL_CONNECTED, status, phrase, NULL, ctx->userdata);
                if (ctx->local_rtp_port > 0 && ctx->remote_rtp_port > 0) {
                    char aux[128];
                    snprintf(aux, sizeof(aux), "%d,%s,%d,%d",
                             ctx->local_rtp_port, ctx->remote_rtp_ip, ctx->remote_rtp_port,
                             ctx->remote_rtp_payload);
                    ctx->cb(SOFIA_EV_CALL_MEDIA, status, phrase, aux, ctx->userdata);
                }
            }
            /* re-INVITE 200 OK (hold/unhold) — already ACK'd, nothing else to do. */
        } else if ((status == 401 || status == 407) && ctx->user && ctx->password) {
            invite_with_digest(ctx, sip, status);
        } else if (status >= 300) {
            ctx->cb(SOFIA_EV_CALL_FAILED, status, phrase, NULL, ctx->userdata);
            if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        }
        break;

    case nua_i_bye:
    case nua_r_bye:
        /* Check consultation handle first */
        if (nh == ctx->consult_nh) {
            if (!ctx->consult_ended)
                ctx->cb(SOFIA_EV_CONSULT_ENDED, status, phrase, NULL, ctx->userdata);
            ctx->consult_remote_port = 0;
            ctx->consult_remote_ip[0] = '\0';
            ctx->consult_established = FALSE;
            ctx->consult_ended = FALSE;
            nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL;
            break;
        }
        /* Primary call */
        ctx->cb(SOFIA_EV_CALL_ENDED, status, phrase, NULL, ctx->userdata);
        ctx->local_rtp_port = 0; ctx->remote_rtp_port = 0;
        ctx->remote_rtp_ip[0] = '\0'; ctx->remote_rtp_payload = 0;
        ctx->call_established = FALSE; ctx->call_on_hold = FALSE;
        if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        break;

    case nua_i_cancel:
        /* Check consultation handle first */
        if (nh == ctx->consult_nh) {
            ctx->cb(SOFIA_EV_CONSULT_ENDED, status, "Cancelled", NULL, ctx->userdata);
            ctx->consult_remote_port = 0;
            ctx->consult_remote_ip[0] = '\0';
            ctx->consult_established = FALSE;
            nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL;
            break;
        }
        ctx->cb(SOFIA_EV_CALL_ENDED, status, "Cancelled", NULL, ctx->userdata);
        ctx->local_rtp_port = 0; ctx->remote_rtp_port = 0;
        ctx->remote_rtp_ip[0] = '\0'; ctx->remote_rtp_payload = 0;
        ctx->call_established = FALSE; ctx->call_on_hold = FALSE;
        if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        break;

    case nua_r_refer:
        if (status == 202 || (status >= 200 && status < 300)) {
            /* REFER accepted — wait for NOTIFY to confirm */
        } else if (status >= 300) {
            ctx->cb(SOFIA_EV_TRANSFER_FAILED, status, phrase, NULL, ctx->userdata);
        }
        break;

    case nua_i_notify:
        /* Respond 200 to the NOTIFY subscription */
        nua_respond(nh, SIP_200_OK, TAG_END());
        if (sip && sip->sip_payload && sip->sip_payload->pl_data) {
            const char *body = sip->sip_payload->pl_data;
            if (strstr(body, "SIP/2.0 2"))       /* 2xx = success */
                ctx->cb(SOFIA_EV_TRANSFER_OK, 200, "Transfer complete", NULL, ctx->userdata);
            else if (strstr(body, "SIP/2.0 4") || strstr(body, "SIP/2.0 5"))
                ctx->cb(SOFIA_EV_TRANSFER_FAILED, 400, "Transfer failed", NULL, ctx->userdata);
        }
        break;

    case nua_r_info:
        /* 200 OK response to our SIP INFO (DTMF) — nothing to do. */
        break;


    case nua_i_error:
        if (nh == ctx->consult_nh) {
            if (!ctx->consult_ended)
                ctx->cb(SOFIA_EV_CONSULT_ENDED, status, phrase, NULL, ctx->userdata);
            ctx->consult_established = FALSE;
            ctx->consult_ended = FALSE;
            nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL;
        } else if (nh == ctx->call_nh) {
            ctx->cb(SOFIA_EV_CALL_FAILED, status, phrase, NULL, ctx->userdata);
            nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL;
        }
        /* Errors on stale/unknown handles (e.g. NOTIFY response after the
           dialog was already closed by BYE) are silently discarded. */
        break;

    default:
        break;
    }
}

/* ── Public API ───────────────────────────────────────────────────────────── */

SofiaCtx *sofia_ctx_create(const char *server, int port, const char *proxy,
                           int transport, int tls_verify, const char *tls_ca_file,
                           sofia_event_cb_t cb, void *userdata) {
    su_init();

    SofiaCtx *ctx = (SofiaCtx *)calloc(1, sizeof(*ctx));
    if (!ctx) return NULL;

    ctx->cb        = cb;
    ctx->userdata  = userdata;
    ctx->transport = transport;

    /* Find which local IP the OS would use to reach the SIP server so that
       the Via/Contact headers advertise the correct address on multi-homed
       (e.g. VPN) hosts. */
    strncpy(ctx->local_ip, "0.0.0.0", sizeof(ctx->local_ip));
    if (server && *server)
        get_local_ip_for(server, port, ctx->local_ip, sizeof(ctx->local_ip));

    char nua_url[128];
    if (transport == TRANSPORT_TLS)
        snprintf(nua_url, sizeof(nua_url), "sips:%s:%d", ctx->local_ip, get_free_tcp_port());
    else if (transport == TRANSPORT_TCP)
        snprintf(nua_url, sizeof(nua_url), "sip:%s:%d;transport=tcp", ctx->local_ip, get_free_tcp_port());
    else
        snprintf(nua_url, sizeof(nua_url), "sip:%s:%d", ctx->local_ip, get_free_udp_port());

    ctx->root = su_glib_root_create(NULL);
    if (!ctx->root) {
        free(ctx);
        su_deinit();
        return NULL;
    }

    /* Attach the su_root GLib source to the default main context so that
       NUA application callbacks are dispatched back to the GTK main thread.

       su_glib_root_gsource() returns a borrowed pointer (no extra ref for
       the caller).  In some code paths inside su_glib_root_create(NULL),
       sofia's GLib integration already attaches the source internally and
       drops the creation reference, leaving refcount=1 (held only by the
       context).  Calling g_source_unref() on that would free the source and
       silently detach it from the main loop — sofia events then stop firing
       for the entire session (intermittent at startup because the internal
       path is timing-dependent).

       Fix: if the source is already attached, do a compensating ref before
       the unref so the context's reference is never the one we drop. */
    {
        GSource *src = su_glib_root_gsource(ctx->root);
        if (src) {
            if (!g_source_get_context(src)) {
                g_source_attach(src, NULL);
            } else {
                /* Source already attached — take an extra ref so the unref
                   below cannot be the one that frees the source. */
                g_source_ref(src);
            }
            g_source_unref(src);
        }
    }

    ctx->nua = nua_create(ctx->root, nua_cb, (nua_magic_t *)ctx,
                          NUTAG_URL(nua_url),
                          NUTAG_ALLOW("INVITE, ACK, BYE, CANCEL, OPTIONS, NOTIFY, INFO, REFER"),
                          NUTAG_AUTOACK(0),
                          NUTAG_AUTOANSWER(0),
                          NUTAG_MEDIA_ENABLE(0),  /* manage SDP ourselves */
                          /* Disable RFC 5626 outbound path validation.  Some
                             Asterisk versions respond 404 to the validation
                             probe sent to our contact address, which causes
                             sofia to mark the registration as failed even
                             though the REGISTER itself completed with 200 OK. */
                          NUTAG_OUTBOUND("no-validate no-options-keepalive"),
                          TAG_END());
    if (!ctx->nua) {
        su_root_destroy(ctx->root);
        free(ctx);
        su_deinit();
        return NULL;
    }

    if (transport == TRANSPORT_TLS) {
        /* If a custom CA file is provided, copy it into a temp directory as
           cafile.pem — the name sofia-sip expects — and point NUTAG_CERTIFICATE_DIR
           there.  If no CA file is given and verification is requested, OpenSSL
           falls back to the system CA store automatically. */
        ctx->tls_cert_dir[0] = '\0';
        if (tls_ca_file && *tls_ca_file) {
            char tmpl[] = "/tmp/tmwphone-certs-XXXXXX";
            if (mkdtemp(tmpl)) {
                strncpy(ctx->tls_cert_dir, tmpl, sizeof(ctx->tls_cert_dir) - 1);
                char dst[600];
                snprintf(dst, sizeof(dst), "%s/cafile.pem", ctx->tls_cert_dir);
                FILE *src_f = fopen(tls_ca_file, "rb");
                FILE *dst_f = src_f ? fopen(dst, "wb") : NULL;
                if (src_f && dst_f) {
                    char buf[4096];
                    size_t n;
                    while ((n = fread(buf, 1, sizeof(buf), src_f)) > 0)
                        fwrite(buf, 1, n, dst_f);
                } else {
                    fprintf(stderr, "[tmwphone] TLS: could not copy CA file '%s'\n",
                            tls_ca_file);
                    ctx->tls_cert_dir[0] = '\0';
                }
                if (src_f) fclose(src_f);
                if (dst_f) fclose(dst_f);
            }
        }

        if (ctx->tls_cert_dir[0])
            nua_set_params(ctx->nua,
                           NUTAG_CERTIFICATE_DIR(ctx->tls_cert_dir),
                           TPTAG_TLS_VERIFY_PEER(tls_verify ? 1 : 0),
                           TAG_END());
        else
            nua_set_params(ctx->nua,
                           TPTAG_TLS_VERIFY_PEER(tls_verify ? 1 : 0),
                           TAG_END());
    }

    /* Always route outgoing requests through an explicit next-hop, the same
       way REGISTER pins its destination via NUTAG_REGISTRAR.  Without this,
       sofia resolves each INVITE's request-URI itself through its built-in
       resolver, which fails with "503 DNS Error" on hosts whose resolv.conf
       it cannot parse (e.g. systemd-resolved's "trust-ad" option) — and that
       failure happens even for IP-literal targets that need no DNS at all.

       The route URI MUST carry ";lr" (loose routing).  Without it, sofia
       1.12.x does strict (pre-loaded) routing and splices the proxy host into
       the request-URI, producing a malformed "user@proxy@host" URI that the
       registrar rejects ("syntax error on Request Line"). */
    {
        char proxy_uri[600];
        if (proxy && *proxy) {
            /* User-configured outbound proxy. */
            if (strncmp(proxy, "sip:", 4) == 0 || strncmp(proxy, "sips:", 5) == 0)
                snprintf(proxy_uri, sizeof(proxy_uri), "%s", proxy);
            else if (transport == TRANSPORT_TLS)
                snprintf(proxy_uri, sizeof(proxy_uri), "sips:%s", proxy);
            else if (transport == TRANSPORT_TCP)
                snprintf(proxy_uri, sizeof(proxy_uri), "sip:%s;transport=tcp", proxy);
            else
                snprintf(proxy_uri, sizeof(proxy_uri), "sip:%s", proxy);
        } else if (server && *server) {
            /* No explicit proxy: default to the registrar host:port, so calls
               take the same resolver-free next-hop that REGISTER already uses. */
            snprintf(proxy_uri, sizeof(proxy_uri), "%s:%s:%d%s",
                     sip_scheme(ctx), server, port, transport_param(ctx));
        } else {
            proxy_uri[0] = '\0';
        }

        /* Ensure loose routing so the request-URI is left intact. */
        if (proxy_uri[0] && !strstr(proxy_uri, ";lr"))
            strncat(proxy_uri, ";lr", sizeof(proxy_uri) - strlen(proxy_uri) - 1);

        if (proxy_uri[0]) {
            ctx->proxy = strdup(proxy_uri);
            nua_set_params(ctx->nua, NUTAG_PROXY(ctx->proxy), TAG_END());
        }
    }

    return ctx;
}

void sofia_ctx_destroy(SofiaCtx *ctx) {
    if (!ctx) return;

    if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
    if (ctx->reg_nh)  { nua_handle_unref(ctx->reg_nh);  ctx->reg_nh  = NULL; }
    if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }

    /* nua_destroy requires a completed nua_shutdown first.
       Drain the GLib main context until the shutdown event arrives. */
    ctx->shutting_down = TRUE;
    ctx->shutdown_done = FALSE;
    nua_shutdown(ctx->nua);

    GMainContext *gc = g_main_context_default();
    gint64 deadline = g_get_monotonic_time() + 2 * G_USEC_PER_SEC;
    while (!ctx->shutdown_done && g_get_monotonic_time() < deadline) {
        g_main_context_iteration(gc, FALSE);
    }

    nua_destroy(ctx->nua);
    su_root_destroy(ctx->root);

    if (ctx->tls_cert_dir[0]) {
        char cafile[600];
        snprintf(cafile, sizeof(cafile), "%s/cafile.pem", ctx->tls_cert_dir);
        unlink(cafile);
        rmdir(ctx->tls_cert_dir);
    }

    free(ctx->user);
    free(ctx->password);
    free(ctx->server);
    free(ctx->proxy);
    free(ctx->auth_str);
    free(ctx->call_to);
    free(ctx->cleanup_registrar);
    free(ctx->consult_to);
    free(ctx->consult_call_id);
    free(ctx->consult_from_tag);
    free(ctx->consult_to_tag);
    free(ctx);

    su_deinit();
}

void sofia_register(SofiaCtx   *ctx,
                    const char *server,
                    int         port,
                    const char *user,
                    const char *password,
                    const char *display_name)
{
    free(ctx->user);     ctx->user     = strdup(user);
    free(ctx->password); ctx->password = strdup(password);
    free(ctx->server);   ctx->server   = strdup(server);
    ctx->sip_port = port;

    /* Ensure the username appears in the Contact URI for all handles on this
       NUA context (REGISTER, INVITE, etc.). */
    nua_set_params(ctx->nua, NUTAG_M_USERNAME(ctx->user), TAG_END());

    /* Auth string is populated from the 401 WWW-Authenticate realm;
       pre-fill with server hostname so build_auth has a non-NULL ctx->auth_str
       before the 401 arrives.  The NUA-level NUTAG_AUTH is set in the 401
       handler once the real realm is known. */
    build_auth(ctx, server);

    char registrar[512], from[512];
    snprintf(registrar, sizeof(registrar), "%s:%s:%d%s",
             sip_scheme(ctx), server, port, transport_param(ctx));
    snprintf(from, sizeof(from),
             "\"%s\" <%s:%s@%s>", display_name, sip_scheme(ctx), user, server);

    if (ctx->reg_nh) nua_handle_unref(ctx->reg_nh);
    ctx->reg_nh = nua_handle(ctx->nua, NULL,
                              SIPTAG_FROM_STR(from),
                              SIPTAG_TO_STR(from),
                              TAG_END());

    /* Store registrar URI for use after the startup cleanup completes. */
    free(ctx->cleanup_registrar);
    ctx->cleanup_registrar = strdup(registrar);
    ctx->cleanup_pending   = TRUE;
    ctx->reg_auth_tried    = FALSE; /* fresh auth cycle */

    /* Before the normal REGISTER, remove ALL contacts from previous sessions
       that were never properly unregistered.  Without this, Asterisk
       accumulates stale bindings and routes incoming calls to dead old ports.
       nua_r_unregister fires the response; when 200 OK arrives it calls the
       real nua_register().  On 401 it authenticates and retries.  On any
       error (404, etc.) it also proceeds — nothing to clean up is fine. */
    nua_unregister(ctx->reg_nh,
                   NUTAG_REGISTRAR(registrar),
                   SIPTAG_CONTACT_STR("*"),
                   SIPTAG_EXPIRES_STR("0"),
                   TAG_END());
}

void sofia_unregister(SofiaCtx *ctx) {
    if (ctx->reg_nh) nua_unregister(ctx->reg_nh, TAG_END());
}

void sofia_reregister(SofiaCtx *ctx) {
    if (!ctx->reg_nh) return;
    /* Refresh the registration without resetting any handle-level state.
       Allow one auth attempt for the refresh's challenge. */
    ctx->reg_auth_tried = FALSE;
    nua_register(ctx->reg_nh, TAG_END());
}

void sofia_call(SofiaCtx *ctx, const char *number) {
    if (!ctx->server || !ctx->user) return;

    ctx->call_auth_tried  = FALSE;
    ctx->call_established = FALSE;
    ctx->call_is_incoming = FALSE;
    ctx->call_on_hold     = FALSE;
    ctx->local_rtp_port   = get_free_udp_port();

    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->local_rtp_port);

    char to[512], from[512];
    if (strncmp(number, "sip:", 4) == 0 || strncmp(number, "sips:", 5) == 0)
        snprintf(to, sizeof(to), "%s", number);
    else
        snprintf(to, sizeof(to), "%s:%s@%s", sip_scheme(ctx), number, ctx->server);
    snprintf(from, sizeof(from), "<%s:%s@%s>", sip_scheme(ctx), ctx->user, ctx->server);

    char contact[512];
    build_contact(ctx, contact, sizeof(contact));

    free(ctx->call_to);
    ctx->call_to = strdup(to);   /* saved for use by invite_with_digest */

    if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
    ctx->call_nh = nua_handle(ctx->nua, NULL,
                              SIPTAG_FROM_STR(from),
                              SIPTAG_TO_STR(to),
                              TAG_END());

    /* Explicit Contact: sofia 1.13 otherwise omits it on some paths. */
    nua_invite(ctx->call_nh,
               SIPTAG_CONTACT_STR(contact),
               SIPTAG_CONTENT_TYPE_STR("application/sdp"),
               SIPTAG_PAYLOAD_STR(sdp),
               TAG_END());
}

void sofia_answer(SofiaCtx *ctx) {
    if (!ctx->call_nh) return;

    ctx->call_on_hold   = FALSE;
    ctx->local_rtp_port = get_free_udp_port();
    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->local_rtp_port);

    nua_respond(ctx->call_nh, SIP_200_OK,
                SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                SIPTAG_PAYLOAD_STR(sdp),
                TAG_END());

    ctx->call_established = TRUE;
    ctx->call_is_incoming = FALSE;
    ctx->cb(SOFIA_EV_CALL_CONNECTED, 200, "OK", NULL, ctx->userdata);

    if (ctx->remote_rtp_port > 0) {
        char aux[128];
        snprintf(aux, sizeof(aux), "%d,%s,%d,%d",
                 ctx->local_rtp_port, ctx->remote_rtp_ip, ctx->remote_rtp_port,
                 ctx->remote_rtp_payload);
        ctx->cb(SOFIA_EV_CALL_MEDIA, 200, "OK", aux, ctx->userdata);
    }
}

void sofia_hangup(SofiaCtx *ctx) {
    if (!ctx->call_nh) return;

    if (ctx->call_established) {
        /* Active call — terminate with BYE. */
        nua_bye(ctx->call_nh, TAG_END());
    } else if (ctx->call_is_incoming) {
        /* Ringing inbound call — decline it with a final failure response.
           A BYE is invalid before the dialog is confirmed.  NUA does not
           deliver a callback for our own response, so clean up call state and
           notify the UI here. */
        nua_respond(ctx->call_nh, SIP_603_DECLINE, TAG_END());
        nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL;
        ctx->call_established = FALSE;
        ctx->call_is_incoming = FALSE;
        ctx->call_on_hold = FALSE;
        ctx->local_rtp_port = 0; ctx->remote_rtp_port = 0;
        ctx->remote_rtp_ip[0] = '\0'; ctx->remote_rtp_payload = 0;
        ctx->cb(SOFIA_EV_CALL_ENDED, 603, "Declined", NULL, ctx->userdata);
    } else {
        /* Outbound call not yet answered — cancel it.  The resulting 487 on the
           INVITE drives cleanup via the nua_r_invite (status >= 300) handler. */
        nua_cancel(ctx->call_nh, TAG_END());
    }
}

void sofia_set_hold(SofiaCtx *ctx, int hold) {
    if (!ctx->call_nh || !ctx->call_established) return;
    ctx->call_on_hold = hold ? TRUE : FALSE;
    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp),
                    hold ? "sendonly" : "sendrecv", ctx->local_rtp_port);
    char contact[512];
    build_contact(ctx, contact, sizeof(contact));
    nua_invite(ctx->call_nh,
               SIPTAG_CONTACT_STR(contact),
               SIPTAG_CONTENT_TYPE_STR("application/sdp"),
               SIPTAG_PAYLOAD_STR(sdp),
               TAG_END());
}

void sofia_send_dtmf(SofiaCtx *ctx, char digit) {
    if (!ctx->call_nh) return;
    char body[48];
    snprintf(body, sizeof(body), "Signal=%c\r\nDuration=160\r\n", digit);
    nua_info(ctx->call_nh,
             SIPTAG_CONTENT_TYPE_STR("application/dtmf-relay"),
             SIPTAG_PAYLOAD_STR(body),
             TAG_END());
}

void sofia_blind_transfer(SofiaCtx *ctx, const char *number) {
    if (!ctx->call_nh || !ctx->call_established) return;
    char to[512];
    if (strncmp(number, "sip:", 4) == 0 || strncmp(number, "sips:", 5) == 0)
        snprintf(to, sizeof(to), "<%s>", number);
    else
        snprintf(to, sizeof(to), "<%s:%s@%s>", sip_scheme(ctx), number, ctx->server);
    nua_refer(ctx->call_nh, SIPTAG_REFER_TO_STR(to), TAG_END());
}

void sofia_start_consultation(SofiaCtx *ctx, const char *number) {
    if (!ctx->call_established) return;
    /* Put primary call on hold */
    sofia_set_hold(ctx, 1);

    ctx->consult_established = FALSE;
    ctx->consult_auth_tried  = FALSE;
    ctx->consult_local_rtp_port = get_free_udp_port();

    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp), "sendrecv", ctx->consult_local_rtp_port);

    char to[512], from[512];
    if (strncmp(number, "sip:", 4) == 0 || strncmp(number, "sips:", 5) == 0)
        snprintf(to, sizeof(to), "%s", number);
    else
        snprintf(to, sizeof(to), "%s:%s@%s", sip_scheme(ctx), number, ctx->server);
    snprintf(from, sizeof(from), "<%s:%s@%s>", sip_scheme(ctx), ctx->user, ctx->server);

    free(ctx->consult_to);
    ctx->consult_to = strdup(to);

    char contact[512];
    build_contact(ctx, contact, sizeof(contact));

    if (ctx->consult_nh) { nua_handle_unref(ctx->consult_nh); ctx->consult_nh = NULL; }
    ctx->consult_nh = nua_handle(ctx->nua, NULL,
                                  SIPTAG_FROM_STR(from),
                                  SIPTAG_TO_STR(to),
                                  TAG_END());
    nua_invite(ctx->consult_nh,
               SIPTAG_CONTACT_STR(contact),
               SIPTAG_CONTENT_TYPE_STR("application/sdp"),
               SIPTAG_PAYLOAD_STR(sdp),
               TAG_END());
}

void sofia_complete_transfer(SofiaCtx *ctx) {
    if (!ctx->call_nh || !ctx->consult_to) return;

    char refer_to[1024];
    if (ctx->consult_call_id && ctx->consult_from_tag && ctx->consult_to_tag) {
        /* Attended transfer: Refer-To includes a Replaces parameter so 886
           atomically swaps from the consult dialog to a new dialog with 20.
           Semicolons must be %-encoded inside a SIP URI query component. */
        char enc_cid[512] = {0};
        for (const char *p = ctx->consult_call_id; *p; p++) {
            if      (*p == ';') strncat(enc_cid, "%3B", sizeof(enc_cid) - strlen(enc_cid) - 1);
            else if (*p == '@') strncat(enc_cid, "%40", sizeof(enc_cid) - strlen(enc_cid) - 1);
            else { char s[2] = {*p, 0}; strncat(enc_cid, s, sizeof(enc_cid) - strlen(enc_cid) - 1); }
        }
        snprintf(refer_to, sizeof(refer_to),
                 "<%s?Replaces=%s%%3Bfrom-tag%%3D%s%%3Bto-tag%%3D%s>",
                 ctx->consult_to, enc_cid,
                 ctx->consult_from_tag, ctx->consult_to_tag);
    } else {
        /* No dialog IDs (consult never fully established) — fall back to
           a plain blind transfer. */
        snprintf(refer_to, sizeof(refer_to), "<%s>", ctx->consult_to);
    }

    nua_refer(ctx->call_nh, SIPTAG_REFER_TO_STR(refer_to), TAG_END());

    /* Do NOT BYE consult_nh here.  With Replaces, 886 atomically terminates
       the consultation leg itself when it accepts the INVITE from 20.  We let
       nua_i_bye / nua_r_bye on consult_nh clean up once that BYE arrives.
       Sending BYE here in parallel with REFER causes a race that drops the
       transferred call immediately. */
}

void sofia_cancel_consultation(SofiaCtx *ctx) {
    if (!ctx->consult_nh) return;

    if (ctx->consult_established)
        nua_bye(ctx->consult_nh, TAG_END());
    else
        nua_cancel(ctx->consult_nh, TAG_END());

    /* Keep consult_nh alive so the BYE/487 response callback can identify it
       and clean up. Set consult_ended so that callback suppresses a duplicate
       CONSULT_ENDED event. */
    ctx->consult_ended = TRUE;
    ctx->consult_established = FALSE;

    /* Resume primary call and notify UI immediately. */
    sofia_set_hold(ctx, 0);
    ctx->cb(SOFIA_EV_CONSULT_ENDED, 0, "Cancelled", NULL, ctx->userdata);
}
