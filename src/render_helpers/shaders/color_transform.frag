
// Conversion of surface contents into the blend space, see src/color.rs.

uniform float niri_cm_tf;
uniform float niri_cm_tf_param;
uniform float niri_cm_luminance_scale;
uniform vec3 niri_cm_luma;
uniform mat3 niri_cm_matrix;
// x: knee, y: compression coefficient. Disabled when the knee is negative.
uniform vec2 niri_cm_tone_mapping;
uniform float niri_cm_max;

// Decodes to linear, normalized to the [0, 1] range of the transfer function.
vec3 niri_cm_eotf(vec3 e) {
    if (niri_cm_tf == 1.0) {
        // Power curve, mirrored for negative values.
        return sign(e) * pow(abs(e), vec3(niri_cm_tf_param));
    } else if (niri_cm_tf == 2.0) {
        // sRGB piece-wise, mirrored for negative values.
        vec3 a = abs(e);
        vec3 lin = mix(a / 12.92, pow((a + 0.055) / 1.055, vec3(2.4)), step(0.04045, a));
        return sign(e) * lin;
    } else if (niri_cm_tf == 3.0) {
        // SMPTE ST 2084 (PQ).
        const float m1 = 0.1593017578125;
        const float m2 = 78.84375;
        const float c1 = 0.8359375;
        const float c2 = 18.8515625;
        const float c3 = 18.6875;
        vec3 p = pow(clamp(e, 0.0, 1.0), vec3(1.0 / m2));
        return pow(max(p - c1, 0.0) / (c2 - c3 * p), vec3(1.0 / m1));
    } else if (niri_cm_tf == 4.0) {
        // HLG inverse OETF to scene linear; the OOTF is applied by the caller.
        const float a = 0.17883277;
        const float b = 0.28466892;
        const float c = 0.55991073;
        vec3 x = clamp(e, 0.0, 1.0);
        vec3 low = x * x / 3.0;
        vec3 high = (exp((x - c) / a) + b) / 12.0;
        return mix(low, high, step(0.5, x));
    }

    // Linear.
    return e;
}

vec4 niri_color(vec4 color) {
    if (color.a <= 0.0) {
        return vec4(0.0);
    }

    // Textures are premultiplied, but transfer functions apply to straight color.
    vec3 rgb = color.rgb / color.a;

    rgb = niri_cm_eotf(rgb);
    if (niri_cm_tf == 4.0) {
        // HLG OOTF.
        float ys = dot(niri_cm_luma, rgb);
        if (ys > 0.0) {
            rgb = rgb * pow(ys, niri_cm_tf_param);
        }
    }

    // Now relative to reference white.
    rgb = niri_cm_luminance_scale * rgb;
    rgb = niri_cm_matrix * rgb;

    if (niri_cm_tone_mapping.x >= 0.0) {
        float knee = niri_cm_tone_mapping.x;
        float m = max(max(rgb.r, rgb.g), rgb.b);
        if (m > knee) {
            float excess = m - knee;
            float mapped = knee + excess / (1.0 + niri_cm_tone_mapping.y * excess);
            rgb = rgb * (mapped / m);
        }
    }

    rgb = min(rgb, vec3(niri_cm_max));

    // Encode into the blend space.
    rgb = sign(rgb) * pow(abs(rgb), vec3(1.0 / 2.2));

    return vec4(rgb * color.a, color.a);
}
