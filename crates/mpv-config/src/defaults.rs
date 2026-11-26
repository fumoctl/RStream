pub const DEFAULT_MPV_CONF: &str = r#"# RStream mpv configuration
# Embedded-surface and security keys (vo, wid, force-window, gpu-api, gpu-context,
# osc, config, config-dir, include, scripts, load-scripts, http-proxy, tls-verify, etc.)
# are managed by RStream and ignored if set here.

# Example options (uncomment to enable):
# deband=yes
# hwdec=auto-safe
# slang=en,eng
# glsl-shaders=~~/shaders/cas-sharpen.glsl
"#;

pub const DEFAULT_INPUT_CONF: &str = r##"# RStream mpv input configuration
# Use "#menu: Group > Label" or "# Label" to customize the in-player Commands menu.
# Built-in player keys (Space, arrows, 0-9 seek, m, f fullscreen, s subtitles, z fit,
# ,/. sub delay, [/]/\ speed, Backspace/Esc) are handled natively unless overridden here.

# SPACE cycle pause                                      #menu: Playback > Toggle Pause
# LEFT seek -5                                           #menu: Playback > Seek -5s
# RIGHT seek 5                                           #menu: Playback > Seek +5s
# UP add volume 5                                        #menu: Audio > Volume +5
# DOWN add volume -5                                     #menu: Audio > Volume -5
# m cycle mute                                           #menu: Audio > Toggle Mute
# f cycle fullscreen                                     #menu: Video > Toggle Fullscreen
# BS set speed 1.0                                       #menu: Playback > Reset Speed (1.0x)

Alt+. frame-step                                         #menu: Playback > Frame Step
Alt+, frame-back-step                                    #menu: Playback > Frame Back Step

Ctrl+- add audio-delay -0.1                              #menu: Audio > Audio Delay -0.1s
Ctrl++ add audio-delay 0.1                               #menu: Audio > Audio Delay +0.1s
Ctrl+0 set audio-delay 0                                 #menu: Audio > Reset Audio Delay

Alt+- add sub-delay -0.1                                 #menu: Subtitles > Subtitle Delay -0.1s
x add sub-delay 0.1                                      #menu: Subtitles > Subtitle Delay +0.1s
v cycle sub-visibility                                   #menu: Subtitles > Toggle Subtitles

d cycle deband                                           #menu: Video > Toggle Deband
A cycle-values video-aspect-override "16:9" "4:3" "2.35:1" "-1" #menu: Video > Cycle Aspect Ratio
W add panscan 0.1                                        #menu: Video > Panscan +0.1
w add panscan -0.1                                       #menu: Video > Panscan -0.1
Ctrl+s screenshot                                        #menu: Video > Take Screenshot

Alt+1 change-list glsl-shaders toggle "~~/shaders/cas-sharpen.glsl"  #menu: Shaders > Toggle CAS Sharpening
Alt+2 change-list glsl-shaders toggle "~~/shaders/cinema-warm.glsl"  #menu: Shaders > Toggle Cinema Warmth
Alt+0 change-list glsl-shaders clr ""                                #menu: Shaders > Clear All Shaders
"##;

pub const SHADER_CAS_SHARPEN: &str = r#"//!HOOK MAIN
//!BIND HOOKED
//!DESC Contrast Adaptive Sharpening

vec4 hook() {
    float sharpness = 0.5;
    vec4 center = HOOKED_tex(HOOKED_pos);
    vec3 a = HOOKED_tex(HOOKED_pos + vec2( 0.0, -1.0) * HOOKED_pt).rgb;
    vec3 b = HOOKED_tex(HOOKED_pos + vec2(-1.0,  0.0) * HOOKED_pt).rgb;
    vec3 c = center.rgb;
    vec3 d = HOOKED_tex(HOOKED_pos + vec2( 1.0,  0.0) * HOOKED_pt).rgb;
    vec3 e = HOOKED_tex(HOOKED_pos + vec2( 0.0,  1.0) * HOOKED_pt).rgb;

    vec3 mn = min(min(a, b), min(min(d, e), c));
    vec3 mx = max(max(a, b), max(max(d, e), c));

    vec3 amp = clamp(min(mn, 1.0 - mx) / max(mx, vec3(1e-5)), 0.0, 1.0);
    amp = sqrt(amp);
    vec3 w = amp * mix(-0.125, -0.2, sharpness);

    vec3 out_rgb = (c + (a + b + d + e) * w) / (1.0 + 4.0 * w);
    return vec4(clamp(out_rgb, 0.0, 1.0), center.a);
}
"#;

pub const SHADER_CINEMA_WARM: &str = r#"//!HOOK MAIN
//!BIND HOOKED
//!DESC Subtle Cinema Warmth

vec4 hook() {
    vec4 center = HOOKED_tex(HOOKED_pos);
    vec3 col = clamp(center.rgb, 0.0, 1.0);

    vec3 warm = col * vec3(1.03, 1.005, 0.96);
    vec3 soft_s = warm * warm * (3.0 - 2.0 * warm);
    vec3 out_rgb = mix(warm, soft_s, 0.12);

    return vec4(clamp(out_rgb, 0.0, 1.0), center.a);
}
"#;
