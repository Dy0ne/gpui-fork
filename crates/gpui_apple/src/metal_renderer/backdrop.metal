// --- backdrop layers --- //
//
// Fork-only, kept out of the upstream `shaders.metal`: build.rs appends this file to it, so it
// can use that file's types and helpers and the generated `scene.h`.
//
// A backdrop layer blurs what was rendered behind it and draws it back through an effect. The
// renderer runs four passes per layer: downsample the scene to half resolution, two separable
// gaussian passes, and a composite into the layer's rounded rect. Frames with backdrop layers
// render offscreen, and `backdrop_copy_fragment` copies the result to the drawable.

// Mirrors the Rust `BackdropParams` byte for byte; keep the fields in sync. `tint` is four
// floats rather than a float4, whose 16-byte alignment would insert padding.
struct BackdropParams {
  Bounds_ScaledPixels bounds;
  Bounds_ScaledPixels content_mask;
  Corners_ScaledPixels corner_radii;
  float2 blur_direction;
  float blur_sigma;
  float blur_tap_count;
  float blur_tap_step;
  float opacity;
  float effect;
  float refraction;
  float rim;
  float magnify;
  float aberration;
  float tint[4];
  float rim_tone;
};

struct BackdropFullscreenOutput {
  float4 position [[position]];
};

vertex BackdropFullscreenOutput backdrop_fullscreen_vertex(
    uint unit_vertex_id [[vertex_id]],
    constant float2 *unit_vertices [[buffer(0)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  BackdropFullscreenOutput output;
  output.position = float4(unit_vertex.x * 2. - 1., 1. - unit_vertex.y * 2., 0., 1.);
  return output;
}

fragment float4 backdrop_downsample_fragment(BackdropFullscreenOutput input [[stage_in]],
                                             texture2d<float> source [[texture(0)]]) {
  constexpr sampler linear_sampler(mag_filter::linear, min_filter::linear);
  float2 source_size = float2(float(source.get_width()), float(source.get_height()));
  // One bilinear tap at the center of the 2x2 block under this half-resolution pixel
  // averages the block.
  float2 uv = (floor(input.position.xy) * 2. + 1.) / source_size;
  return source.sample(linear_sampler, uv);
}

fragment float4 backdrop_blur_fragment(BackdropFullscreenOutput input [[stage_in]],
                                       texture2d<float> source [[texture(0)]],
                                       constant BackdropParams &params [[buffer(1)]]) {
  constexpr sampler linear_sampler(mag_filter::linear, min_filter::linear);
  float2 source_size = float2(float(source.get_width()), float(source.get_height()));
  float2 uv = input.position.xy / source_size;
  int taps = int(params.blur_tap_count);
  float4 color = float4(0.);
  float weight_sum = 0.;
  for (int tap = -taps; tap <= taps; tap++) {
    float offset = float(tap) * params.blur_tap_step;
    float weight = gaussian(offset, params.blur_sigma);
    color += source.sample(linear_sampler, uv + params.blur_direction * offset) * weight;
    weight_sum += weight;
  }
  return color / max(weight_sum, 1e-5);
}

fragment float4 backdrop_copy_fragment(BackdropFullscreenOutput input [[stage_in]],
                                       texture2d<float> source [[texture(0)]]) {
  return source.read(uint2(input.position.xy));
}

struct BackdropCompositeOutput {
  float4 position [[position]];
  float clip_distance [[clip_distance]][4];
};

struct BackdropCompositeFragmentInput {
  float4 position [[position]];
};

vertex BackdropCompositeOutput backdrop_composite_vertex(
    uint unit_vertex_id [[vertex_id]],
    constant float2 *unit_vertices [[buffer(0)]],
    constant BackdropParams &params [[buffer(1)]],
    constant Size_DevicePixels *viewport_size [[buffer(2)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  BackdropCompositeOutput output;
  output.position = to_device_position(unit_vertex, params.bounds, viewport_size);
  float4 clip_distance = distance_from_clip_rect(unit_vertex, params.bounds, params.content_mask);
  output.clip_distance[0] = clip_distance.x;
  output.clip_distance[1] = clip_distance.y;
  output.clip_distance[2] = clip_distance.z;
  output.clip_distance[3] = clip_distance.w;
  return output;
}

float backdrop_corner_radius(float2 point, Bounds_ScaledPixels bounds,
                             Corners_ScaledPixels corner_radii) {
  float2 center = float2(bounds.origin.x + bounds.size.width * 0.5,
                         bounds.origin.y + bounds.size.height * 0.5);
  float2 from_center = point - center;
  if (from_center.x < 0.) {
    return from_center.y < 0. ? corner_radii.top_left : corner_radii.bottom_left;
  }
  return from_center.y < 0. ? corner_radii.top_right : corner_radii.bottom_right;
}

// Samples the half-resolution blur at `point` (full-resolution device pixels) shifted by
// `offset`, magnified around `center`.
float4 backdrop_sample(texture2d<float> blurred, float2 point, float2 offset, float2 center,
                       float magnify) {
  constexpr sampler linear_sampler(mag_filter::linear, min_filter::linear);
  float2 half_size = float2(float(blurred.get_width()), float(blurred.get_height()));
  float2 position = center + (point + offset - center) / magnify;
  return blurred.sample(linear_sampler, position / (2. * half_size));
}

// The blurred backdrop bent along the outward normal of the rounded rect by an amount that
// falls off as (1 - t)^2 across the rim, so the edge shows content from outside the shape.
// Then magnification about the center, per-channel aberration, tint and rim light.
float3 liquid_glass(float2 point, texture2d<float> blurred, constant BackdropParams &params,
                    float distance) {
  Bounds_ScaledPixels bounds = params.bounds;
  Corners_ScaledPixels corner_radii = params.corner_radii;
  float2 center = float2(bounds.origin.x + bounds.size.width * 0.5,
                         bounds.origin.y + bounds.size.height * 0.5);

  // Within the corner radius of the edge the distance field's normal is continuous, so
  // capping the rim there keeps the refraction free of seams.
  float rim = min(params.rim, backdrop_corner_radius(point, bounds, corner_radii));
  float depth = rim > 0. ? saturate(-distance / rim) : 1.;
  float edge = (1. - depth) * (1. - depth);

  float2 gradient = float2(
      quad_sdf(point + float2(1., 0.), bounds, corner_radii) -
          quad_sdf(point - float2(1., 0.), bounds, corner_radii),
      quad_sdf(point + float2(0., 1.), bounds, corner_radii) -
          quad_sdf(point - float2(0., 1.), bounds, corner_radii));
  float gradient_length = length(gradient);
  float2 normal = gradient_length > 1e-4 ? gradient / gradient_length : float2(0.);
  float2 shift = normal * (params.refraction * edge);

  float magnify = max(params.magnify, 0.01);
  float aberration = params.aberration;
  float3 color;
  if (length(shift) * aberration > 0.05) {
    color = float3(
        backdrop_sample(blurred, point, shift * (1. + aberration), center, magnify).r,
        backdrop_sample(blurred, point, shift, center, magnify).g,
        backdrop_sample(blurred, point, shift * (1. - aberration), center, magnify).b);
  } else {
    color = backdrop_sample(blurred, point, shift, center, magnify).rgb;
  }

  float3 tint = float3(params.tint[0], params.tint[1], params.tint[2]);
  color = mix(color, tint, params.tint[3]);

  // Light from the top left. Squaring the dot product also lights the opposite rim, and the
  // constant adds a faint glow all around, since glass reflects more at grazing angles.
  float facing = dot(normal, float2(-0.6, -0.8));
  float light = edge * (1. - depth) * (facing * facing * 0.55 + 0.2) *
                min(abs(params.rim_tone), 1.);
  return mix(color, float3(params.rim_tone >= 0. ? 1. : 0.), light);
}

fragment float4 backdrop_composite_fragment(BackdropCompositeFragmentInput input [[stage_in]],
                                            texture2d<float> blurred [[texture(0)]],
                                            constant BackdropParams &params [[buffer(1)]]) {
  float2 point = input.position.xy;
  float distance = quad_sdf(point, params.bounds, params.corner_radii);
  float3 color;
  if (params.effect > 0.5) {
    color = liquid_glass(point, blurred, params, distance);
  } else {
    float2 center = float2(params.bounds.origin.x + params.bounds.size.width * 0.5,
                           params.bounds.origin.y + params.bounds.size.height * 0.5);
    color = backdrop_sample(blurred, point, float2(0.), center, 1.).rgb;
  }
  return float4(color, saturate(0.5 - distance) * params.opacity);
}
