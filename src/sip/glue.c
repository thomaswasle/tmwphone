#include "glue.h"

#include <sofia-sip/nua.h>
#include <sofia-sip/su_glib.h>
#include <sofia-sip/sip_status.h>
#include <sofia-sip/sdp.h>
#include <sofia-sip/auth_digest.h>

#include <glib.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
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
    char             *auth_str;
    char             *call_to;    /* To URI of the current outgoing call */
    char              local_ip[INET_ADDRSTRLEN];

    int               local_rtp_port;
    char              remote_rtp_ip[64];
    int               remote_rtp_port;
    int               remote_rtp_payload; /* selected RTP payload type (0=PCMU, 8=PCMA) */

    gboolean          call_auth_tried;  /* true after first digest attempt */
    gboolean          shutting_down;
    gboolean          shutdown_done;
};

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

/* Extract the first audio stream's connection address, port, and selected
   payload type from the SDP body of a SIP message. */
static void extract_rtp_from_sip(SofiaCtx *ctx, sip_t const *sip) {
    if (!sip || !sip->sip_payload || !sip->sip_payload->pl_data) {
        fprintf(stderr, "[glue] extract_rtp_from_sip: no SDP payload\n");
        return;
    }

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
            if (m_addr)
                strncpy(ctx->remote_rtp_ip, m_addr, sizeof(ctx->remote_rtp_ip) - 1);
            ctx->remote_rtp_port = (int)m->m_port;

            /* Pick the first non-telephone-event payload type as the codec. */
            ctx->remote_rtp_payload = 0; /* default PCMU */
            for (sdp_rtpmap_t const *rm = m->m_rtpmaps; rm; rm = rm->rm_next) {
                if (rm->rm_pt != 101) {
                    ctx->remote_rtp_payload = (int)rm->rm_pt;
                    break;
                }
            }
            break;
        }
    } else {
        fprintf(stderr, "[glue] extract_rtp_from_sip: SDP parse failed\n");
    }

    fprintf(stderr, "[glue] rtp: remote=%s:%d payload=%d local_port=%d\n",
            ctx->remote_rtp_ip, ctx->remote_rtp_port,
            ctx->remote_rtp_payload, ctx->local_rtp_port);

    su_home_deinit(home);
}

/* Build an SDP offer/answer for ctx->local_ip:port.
   Offer PCMU + PCMA + telephone-event for broad Asterisk compatibility. */
static void build_audio_sdp(SofiaCtx *ctx, char *buf, size_t len) {
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
        "a=sendrecv\r\n",
        ctx->local_ip, ctx->local_ip, ctx->local_rtp_port);
}

/* ── Auth helpers ─────────────────────────────────────────────────────────── */

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

/* ── Digest auth (manual, bypasses nua_authenticate which is broken in
      libsofia-sip-ua 1.12.11) ─────────────────────────────────────────── */

/* Compute digest response and re-send INVITE with Authorization header.
   Called from nua_r_invite when status is 401 or 407. */
static void invite_with_digest(SofiaCtx *ctx, sip_t const *sip, int status)
{
    if (!sip) return;

    /* Only attempt auth once per call to prevent infinite retry loops. */
    if (ctx->call_auth_tried) {
        fprintf(stderr, "[glue] invite_with_digest: already tried once, giving up\n");
        ctx->cb(SOFIA_EV_CALL_FAILED, status, "Authentication failed", NULL, ctx->userdata);
        if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        return;
    }
    ctx->call_auth_tried = TRUE;

    msg_auth_t const *ch = (status == 401)
        ? sip->sip_www_authenticate
        : sip->sip_proxy_authenticate;
    if (!ch || !ch->au_params) return;

    su_home_t home[1];
    su_home_init(home);

    auth_challenge_t ac = { sizeof(ac) };
    auth_digest_challenge_get(home, &ac, ch->au_params);

    const char *realm = ac.ac_realm ? ac.ac_realm : ctx->server;
    const char *nonce = ac.ac_nonce ? ac.ac_nonce : "";

    /* Update stored realm for future operations */
    build_auth(ctx, realm);

    /* Use the To URI that was set when the call was initiated — don't re-parse
       from the 401 response where su_home ownership makes pointer lifetimes
       tricky and produces off-by-one corruptions in url_user. */
    const char *to_uri = ctx->call_to ? ctx->call_to : "";
    char to_hdr[520];
    snprintf(to_hdr, sizeof(to_hdr), "<%s>", to_uri);

    /* Compute HA1 = MD5(user:realm:password) */
    auth_hexmd5_t ha1, hexresp;
    auth_digest_ha1(ha1, ctx->user, realm, ctx->password);

    /* Fill response parameters; handle qop=auth if Asterisk sends it */
    auth_response_t ar = { sizeof(ar) };
    ar.ar_realm  = realm;
    ar.ar_nonce  = nonce;
    ar.ar_uri    = to_uri;
    const char *cnonce = "4b6f63616c20";
    const char *nc     = "00000001";
    if (ac.ac_auth) {
        ar.ar_qop    = "auth";
        ar.ar_cnonce = cnonce;
        ar.ar_nc     = nc;
        ar.ar_auth   = 1;
    }
    auth_digest_response(&ar, hexresp, ha1, "INVITE", NULL, 0);

    /* Build Authorization / Proxy-Authorization header value.
       su_home_deinit must come AFTER we're done with realm, nonce, ac.*. */
    char auth_hdr[2048];
    int n = snprintf(auth_hdr, sizeof(auth_hdr),
        "Digest username=\"%s\", realm=\"%s\", nonce=\"%s\","
        " uri=\"%s\", response=\"%s\", algorithm=MD5",
        ctx->user, realm, nonce, to_uri, hexresp);
    if (ac.ac_auth && n > 0 && n < (int)sizeof(auth_hdr))
        n += snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
            ", qop=auth, nc=%s, cnonce=\"%s\"", nc, cnonce);
    if (ac.ac_opaque && n > 0 && n < (int)sizeof(auth_hdr))
        snprintf(auth_hdr + n, sizeof(auth_hdr) - n,
            ", opaque=\"%s\"", ac.ac_opaque);

    fprintf(stderr, "[glue] digest: user=%s realm=%s uri=%s resp=%s\n",
            ctx->user, realm, to_uri, hexresp);

    su_home_deinit(home);  /* ac.* pointers are invalid after this */

    /* New handle + fresh SDP port, re-send INVITE with credentials */
    char from_hdr[256];
    snprintf(from_hdr, sizeof(from_hdr), "<sip:%s@%s>", ctx->user, ctx->server);
    if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
    ctx->call_nh = nua_handle(ctx->nua, NULL,
                              SIPTAG_FROM_STR(from_hdr),
                              SIPTAG_TO_STR(to_hdr),
                              TAG_END());

    ctx->local_rtp_port = get_free_udp_port();
    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp));

    if (status == 401)
        nua_invite(ctx->call_nh,
                   SIPTAG_AUTHORIZATION_STR(auth_hdr),
                   SIPTAG_CONTENT_TYPE_STR("application/sdp"),
               SIPTAG_PAYLOAD_STR(sdp),
                   TAG_END());
    else
        nua_invite(ctx->call_nh,
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
            /* Store credentials globally so NUA auto-retries any 401/407
               challenge (INVITE, etc.) without needing a callback round-trip. */
            nua_set_params(ctx->nua, NUTAG_AUTH(ctx->auth_str), TAG_END());
            ctx->cb(SOFIA_EV_REGISTER_OK, status, phrase, NULL, ctx->userdata);
        } else if ((status == 401 || status == 407) && ctx->user && ctx->password) {
            msg_auth_t const *challenge = (status == 401)
                ? sip->sip_www_authenticate
                : sip->sip_proxy_authenticate;
            char *realm = extract_realm(challenge);
            build_auth(ctx, realm ? realm : ctx->server);
            free(realm);
            nua_authenticate(nh, NUTAG_AUTH(ctx->auth_str), TAG_END());
        } else if (status >= 300) {
            ctx->cb(SOFIA_EV_REGISTER_FAIL, status, phrase, NULL, ctx->userdata);
        }
        break;

    case nua_r_unregister:
        break;

    case nua_i_invite: {
        if (ctx->call_nh && ctx->call_nh != nh) nua_handle_unref(ctx->call_nh);
        ctx->call_nh = nua_handle_ref(nh);

        /* Store caller's SDP offer so sofia_answer() can include it later. */
        extract_rtp_from_sip(ctx, sip);

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
        ctx->cb(SOFIA_EV_INCOMING_CALL, status, phrase,
                from_buf[0] ? from_buf : "Unknown", ctx->userdata);
        break;
    }

    case nua_r_invite:
        if (status >= 200 && status < 300) {
            nua_ack(nh, TAG_END());
            extract_rtp_from_sip(ctx, sip);
            ctx->cb(SOFIA_EV_CALL_CONNECTED, status, phrase, NULL, ctx->userdata);
            if (ctx->local_rtp_port > 0 && ctx->remote_rtp_port > 0) {
                char aux[128];
                snprintf(aux, sizeof(aux), "%d,%s,%d,%d",
                         ctx->local_rtp_port, ctx->remote_rtp_ip, ctx->remote_rtp_port,
                         ctx->remote_rtp_payload);
                ctx->cb(SOFIA_EV_CALL_MEDIA, status, phrase, aux, ctx->userdata);
            }
        } else if ((status == 401 || status == 407) && ctx->user && ctx->password) {
            invite_with_digest(ctx, sip, status);
        } else if (status >= 300) {
            ctx->cb(SOFIA_EV_CALL_FAILED, status, phrase, NULL, ctx->userdata);
            if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        }
        break;

    case nua_i_bye:
    case nua_r_bye:
        ctx->cb(SOFIA_EV_CALL_ENDED, status, phrase, NULL, ctx->userdata);
        ctx->local_rtp_port = 0; ctx->remote_rtp_port = 0;
        ctx->remote_rtp_ip[0] = '\0'; ctx->remote_rtp_payload = 0;
        if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        break;

    case nua_i_cancel:
        ctx->cb(SOFIA_EV_CALL_ENDED, status, "Cancelled", NULL, ctx->userdata);
        ctx->local_rtp_port = 0; ctx->remote_rtp_port = 0;
        ctx->remote_rtp_ip[0] = '\0'; ctx->remote_rtp_payload = 0;
        if (ctx->call_nh == nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
        break;

    case nua_r_info:
        /* 200 OK response to our SIP INFO (DTMF) — nothing to do. */
        break;

    case nua_i_error:
        ctx->cb(SOFIA_EV_CALL_FAILED, status, phrase, NULL, ctx->userdata);
        break;

    default:
        break;
    }
}

/* ── Public API ───────────────────────────────────────────────────────────── */

SofiaCtx *sofia_ctx_create(const char *server, int port,
                           sofia_event_cb_t cb, void *userdata) {
    su_init();

    SofiaCtx *ctx = (SofiaCtx *)calloc(1, sizeof(*ctx));
    if (!ctx) return NULL;

    ctx->cb       = cb;
    ctx->userdata = userdata;

    /* Find which local IP the OS would use to reach the SIP server so that
       the Via/Contact headers advertise the correct address on multi-homed
       (e.g. VPN) hosts. */
    strncpy(ctx->local_ip, "0.0.0.0", sizeof(ctx->local_ip));
    if (server && *server)
        get_local_ip_for(server, port, ctx->local_ip, sizeof(ctx->local_ip));

    char nua_url[64];
    snprintf(nua_url, sizeof(nua_url), "sip:%s:0", ctx->local_ip);

    ctx->root = su_glib_root_create(NULL);
    if (!ctx->root) {
        free(ctx);
        su_deinit();
        return NULL;
    }

    /* Attach the su_root GLib source to the default main context so that
       NUA application callbacks are dispatched back to the GTK main thread.
       su_glib_root_create(NULL) creates the source but does not attach it. */
    {
        GSource *src = su_glib_root_gsource(ctx->root);
        if (src) {
            if (!g_source_get_context(src))
                g_source_attach(src, NULL);
            g_source_unref(src);
        }
    }

    ctx->nua = nua_create(ctx->root, nua_cb, (nua_magic_t *)ctx,
                          NUTAG_URL(nua_url),
                          NUTAG_ALLOW("INVITE, ACK, BYE, CANCEL, OPTIONS, NOTIFY, INFO"),
                          NUTAG_AUTOACK(0),
                          NUTAG_AUTOANSWER(0),
                          NUTAG_MEDIA_ENABLE(0),  /* manage SDP ourselves */
                          TAG_END());
    if (!ctx->nua) {
        su_root_destroy(ctx->root);
        free(ctx);
        su_deinit();
        return NULL;
    }

    return ctx;
}

void sofia_ctx_destroy(SofiaCtx *ctx) {
    if (!ctx) return;

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

    free(ctx->user);
    free(ctx->password);
    free(ctx->server);
    free(ctx->auth_str);
    free(ctx->call_to);
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

    /* Build tentative auth string using server hostname as realm;
       will be corrected from the WWW-Authenticate realm on first 401. */
    build_auth(ctx, server);

    char registrar[512], from[512];
    snprintf(registrar, sizeof(registrar), "sip:%s:%d", server, port);
    snprintf(from,      sizeof(from),
             "\"%s\" <sip:%s@%s>", display_name, user, server);

    if (ctx->reg_nh) nua_handle_unref(ctx->reg_nh);
    ctx->reg_nh = nua_handle(ctx->nua, NULL,
                              SIPTAG_FROM_STR(from),
                              SIPTAG_TO_STR(from),
                              TAG_END());

    /* Do not pass SIPTAG_CONTACT_STR — let NUA build it from the actual
       bound address and port.  An explicit "sip:user@ip:0" contact causes
       Asterisk to try delivering INVITEs to port 0, which silently fails. */
    nua_register(ctx->reg_nh,
                 NUTAG_REGISTRAR(registrar),
                 TAG_END());
}

void sofia_unregister(SofiaCtx *ctx) {
    if (ctx->reg_nh) nua_unregister(ctx->reg_nh, TAG_END());
}

void sofia_call(SofiaCtx *ctx, const char *number) {
    if (!ctx->server || !ctx->user) return;

    ctx->call_auth_tried = FALSE;
    ctx->local_rtp_port  = get_free_udp_port();

    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp));

    char to[512], from[512];
    if (strncmp(number, "sip:", 4) == 0 || strncmp(number, "sips:", 5) == 0)
        snprintf(to, sizeof(to), "%s", number);
    else
        snprintf(to, sizeof(to), "sip:%s@%s", number, ctx->server);
    snprintf(from, sizeof(from), "<sip:%s@%s>", ctx->user, ctx->server);

    free(ctx->call_to);
    ctx->call_to = strdup(to);   /* saved for use by invite_with_digest */

    fprintf(stderr, "[glue] sofia_call to=%s from=%s sdp:\n%s\n", to, from, sdp);

    if (ctx->call_nh) { nua_handle_unref(ctx->call_nh); ctx->call_nh = NULL; }
    ctx->call_nh = nua_handle(ctx->nua, NULL,
                              SIPTAG_FROM_STR(from),
                              SIPTAG_TO_STR(to),
                              TAG_END());
    nua_invite(ctx->call_nh,
               SIPTAG_CONTENT_TYPE_STR("application/sdp"),
               SIPTAG_PAYLOAD_STR(sdp),
               TAG_END());
}

void sofia_answer(SofiaCtx *ctx) {
    if (!ctx->call_nh) return;

    ctx->local_rtp_port = get_free_udp_port();
    char sdp[512];
    build_audio_sdp(ctx, sdp, sizeof(sdp));

    nua_respond(ctx->call_nh, SIP_200_OK,
                SIPTAG_CONTENT_TYPE_STR("application/sdp"),
                SIPTAG_PAYLOAD_STR(sdp),
                TAG_END());

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
    if (ctx->call_nh) nua_bye(ctx->call_nh, TAG_END());
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
