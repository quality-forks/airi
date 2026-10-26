#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Encodes the blend space into BT.2100 PQ, see src/color.rs.

// BT.709 to BT.2020.
uniform mat3 niri_matrix;
// Reference white, in PQ normalized luminance.
uniform float niri_reference;
// Maximum output, in PQ normalized luminance.
uniform float niri_max;

vec3 pq_inverse_eotf(vec3 y) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 p = pow(y, vec3(m1));
    return pow((c1 + c2 * p) / (1.0 + c3 * p), vec3(m2));
}

void main() {
    // Premultiplied color over black is the color itself.
    vec4 color = texture2D(tex, v_coords);

    vec3 rgb = sign(color.rgb) * pow(abs(color.rgb), vec3(2.2));
    rgb = niri_matrix * rgb;
    rgb = clamp(rgb * niri_reference, 0.0, niri_max);
    rgb = pq_inverse_eotf(rgb);

    color = vec4(rgb, 1.0) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
