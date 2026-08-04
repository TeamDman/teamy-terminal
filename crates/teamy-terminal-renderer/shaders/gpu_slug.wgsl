// Repository-owned compute shader for the rust-gpu-slug renderer.
// Descriptor ABI is frozen by gpu_slug.rs and validated by build.rs through Naga.

const CELL_WORDS: u32 = 8u;
const GLYPH_WORDS: u32 = 16u;
const BAND_HEADER_WORDS: u32 = 4u;
const COVERAGE_EPSILON: f32 = 0.0000152587890625;

struct U32Buffer {
    words: array<u32>,
}

struct CurveBuffer {
    records: array<vec4<f32>>,
}

@group(0) @binding(0)
var output_image: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(1)
var<storage, read> cells: U32Buffer;
@group(0) @binding(2)
var<storage, read> glyphs: U32Buffer;
@group(0) @binding(3)
var<storage, read> curves: CurveBuffer;
@group(0) @binding(4)
var<storage, read> bands: U32Buffer;
@group(0) @binding(5)
var<storage, read> regions: U32Buffer;
@group(0) @binding(6)
var<storage, read> frame: U32Buffer;

fn unpack_rgba(word: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(word & 255u),
        f32((word >> 8u) & 255u),
        f32((word >> 16u) & 255u),
        f32((word >> 24u) & 255u),
    ) / 255.0;
}

fn saturate(value: f32) -> f32 {
    return clamp(value, 0.0, 1.0);
}

fn quadratic_value(first: f32, control: f32, last: f32, t: f32) -> f32 {
    let inverse = 1.0 - t;
    return inverse * inverse * first + 2.0 * inverse * t * control + t * t * last;
}

fn quadratic_derivative(first: f32, control: f32, last: f32, t: f32) -> f32 {
    return 2.0 * ((control - first) + t * (first - 2.0 * control + last));
}

// x/y contain roots and z contains the valid root count.
fn quadratic_roots(first: f32, control: f32, last: f32) -> vec3<f32> {
    let a = first - 2.0 * control + last;
    let b = 2.0 * (control - first);
    let c = first;
    if (abs(a) <= COVERAGE_EPSILON) {
        if (abs(b) <= COVERAGE_EPSILON) {
            return vec3<f32>(0.0, 0.0, 0.0);
        }
        let root = -c / b;
        if (root >= 0.0 && root < 1.0) {
            return vec3<f32>(root, 0.0, 1.0);
        }
        return vec3<f32>(0.0, 0.0, 0.0);
    }
    let discriminant = b * b - 4.0 * a * c;
    if (discriminant < 0.0) {
        return vec3<f32>(0.0, 0.0, 0.0);
    }
    let root_discriminant = sqrt(max(discriminant, 0.0));
    var first_root = (-b - root_discriminant) / (2.0 * a);
    var second_root = (-b + root_discriminant) / (2.0 * a);
    if (first_root > second_root) {
        let temporary = first_root;
        first_root = second_root;
        second_root = temporary;
    }
    var count = 0.0;
    var output_first = 0.0;
    var output_second = 0.0;
    if (first_root >= 0.0 && first_root < 1.0) {
        output_first = first_root;
        count = 1.0;
    }
    if (second_root >= 0.0 && second_root < 1.0 &&
        (count == 0.0 || abs(second_root - output_first) > COVERAGE_EPSILON)) {
        if (count == 0.0) {
            output_first = second_root;
        } else {
            output_second = second_root;
        }
        count = count + 1.0;
    }
    return vec3<f32>(output_first, output_second, count);
}

fn curve_points(curve_index: u32) -> mat3x2<f32> {
    let first = curves.records[curve_index * 2u];
    let second = curves.records[curve_index * 2u + 1u];
    return mat3x2<f32>(first.xy, first.zw, second.xy);
}

fn curve_extents(points: mat3x2<f32>) -> vec4<f32> {
    var minimum = min(points[0], points[2]);
    var maximum = max(points[0], points[2]);
    let denominator_x = points[0].x - 2.0 * points[1].x + points[2].x;
    if (abs(denominator_x) > COVERAGE_EPSILON) {
        let t = (points[0].x - points[1].x) / denominator_x;
        if (t > 0.0 && t < 1.0) {
            let value = quadratic_value(points[0].x, points[1].x, points[2].x, t);
            minimum.x = min(minimum.x, value);
            maximum.x = max(maximum.x, value);
        }
    }
    let denominator_y = points[0].y - 2.0 * points[1].y + points[2].y;
    if (abs(denominator_y) > COVERAGE_EPSILON) {
        let t = (points[0].y - points[1].y) / denominator_y;
        if (t > 0.0 && t < 1.0) {
            let value = quadratic_value(points[0].y, points[1].y, points[2].y, t);
            minimum.y = min(minimum.y, value);
            maximum.y = max(maximum.y, value);
        }
    }
    return vec4<f32>(minimum, maximum);
}

fn directional_sample(signed_distance: f32, left_ray: bool) -> f32 {
    if (left_ray) {
        return saturate(0.5 - signed_distance);
    }
    return saturate(signed_distance + 0.5);
}

// Returns coverage and edge weight for one curve's horizontal ray crossing.
fn horizontal_sample(points: mat3x2<f32>, sample: vec2<f32>, scale: f32, left_ray: bool) -> vec2<f32> {
    let p0 = points[0] - sample + vec2<f32>(0.0, COVERAGE_EPSILON);
    let p1 = points[1] - sample + vec2<f32>(0.0, COVERAGE_EPSILON);
    let p2 = points[2] - sample + vec2<f32>(0.0, COVERAGE_EPSILON);
    let roots = quadratic_roots(p0.y, p1.y, p2.y);
    var result = vec2<f32>(0.0);
    var index = 0u;
    while (index < u32(roots.z)) {
        let root = select(roots.y, roots.x, index == 0u);
        let derivative = quadratic_derivative(p0.y, p1.y, p2.y, root);
        if (abs(derivative) > COVERAGE_EPSILON) {
            let distance = quadratic_value(p0.x, p1.x, p2.x, root) * scale;
            let direction_sign = select(sign(derivative), -sign(derivative), left_ray);
            result.x = result.x + direction_sign * directional_sample(distance, left_ray);
            result.y = max(result.y, saturate(1.0 - abs(distance) * 2.0));
        }
        index = index + 1u;
    }
    return result;
}

// Returns coverage and edge weight for one curve's vertical ray crossing.
fn vertical_sample(points: mat3x2<f32>, sample: vec2<f32>, scale: f32, left_ray: bool) -> vec2<f32> {
    let p0 = points[0] - sample;
    let p1 = points[1] - sample;
    let p2 = points[2] - sample;
    let roots = quadratic_roots(p0.x, p1.x, p2.x);
    var result = vec2<f32>(0.0);
    var index = 0u;
    while (index < u32(roots.z)) {
        let root = select(roots.y, roots.x, index == 0u);
        let derivative = quadratic_derivative(p0.x, p1.x, p2.x, root);
        if (abs(derivative) > COVERAGE_EPSILON) {
            let distance = quadratic_value(p0.y, p1.y, p2.y, root) * scale;
            let direction_sign = select(sign(derivative), -sign(derivative), left_ray);
            result.x = result.x + direction_sign * directional_sample(distance, left_ray);
            result.y = max(result.y, saturate(1.0 - abs(distance) * 2.0));
        }
        index = index + 1u;
    }
    return result;
}

fn band_index(coordinate: f32, scale: f32, offset: f32, count: u32) -> u32 {
    return u32(clamp(floor(coordinate * scale + offset), 0.0, f32(count - 1u)));
}

fn should_stop(extents: vec4<f32>, coordinate: f32, scale: f32, horizontal: bool, left_ray: bool) -> bool {
    let minimum = select(extents.y, extents.x, horizontal);
    let maximum = select(extents.w, extents.z, horizontal);
    if (left_ray) {
        return (minimum - coordinate) * scale > 0.5;
    }
    return (maximum - coordinate) * scale < -0.5;
}

fn coverage_single(glyph_base: u32, sample: vec2<f32>, scale: f32) -> f32 {
    let curve_start = glyphs.words[glyph_base];
    let curve_count = glyphs.words[glyph_base + 1u];
    if (curve_count == 0u) {
        return 0.0;
    }
    let band_start = glyphs.words[glyph_base + 2u];
    let horizontal_count = glyphs.words[glyph_base + 3u];
    let vertical_count = glyphs.words[glyph_base + 4u];
    let x_scale = bitcast<f32>(glyphs.words[glyph_base + 12u]);
    let y_scale = bitcast<f32>(glyphs.words[glyph_base + 13u]);
    let x_offset = bitcast<f32>(glyphs.words[glyph_base + 14u]);
    let y_offset = bitcast<f32>(glyphs.words[glyph_base + 15u]);

    let horizontal_band = band_index(sample.y, y_scale, y_offset, horizontal_count);
    let horizontal_header = band_start + horizontal_band * BAND_HEADER_WORDS;
    let horizontal_curve_count = bands.words[horizontal_header];
    let horizontal_split = bitcast<f32>(bands.words[horizontal_header + 3u]);
    let horizontal_left = sample.x < horizontal_split;
    let horizontal_list = band_start + select(
        bands.words[horizontal_header + 1u],
        bands.words[horizontal_header + 2u],
        horizontal_left,
    );
    var x_coverage = 0.0;
    var x_weight = 0.0;
    var index = 0u;
    while (index < horizontal_curve_count) {
        let local_curve = bands.words[horizontal_list + index];
        if (local_curve >= curve_count) {
            break;
        }
        let points = curve_points(curve_start + local_curve);
        if (should_stop(curve_extents(points), sample.x, scale, true, horizontal_left)) {
            break;
        }
        let contribution = horizontal_sample(points, sample, scale, horizontal_left);
        x_coverage = x_coverage + contribution.x;
        x_weight = max(x_weight, contribution.y);
        index = index + 1u;
    }

    let vertical_band = band_index(sample.x, x_scale, x_offset, vertical_count);
    let vertical_header = band_start + (horizontal_count + vertical_band) * BAND_HEADER_WORDS;
    let vertical_curve_count = bands.words[vertical_header];
    let vertical_split = bitcast<f32>(bands.words[vertical_header + 3u]);
    let vertical_left = sample.y < vertical_split;
    let vertical_list = band_start + select(
        bands.words[vertical_header + 1u],
        bands.words[vertical_header + 2u],
        vertical_left,
    );
    var y_coverage = 0.0;
    var y_weight = 0.0;
    index = 0u;
    while (index < vertical_curve_count) {
        let local_curve = bands.words[vertical_list + index];
        if (local_curve >= curve_count) {
            break;
        }
        let points = curve_points(curve_start + local_curve);
        if (should_stop(curve_extents(points), sample.y, scale, false, vertical_left)) {
            break;
        }
        let contribution = vertical_sample(points, sample, scale, vertical_left);
        y_coverage = y_coverage + contribution.x;
        y_weight = max(y_weight, contribution.y);
        index = index + 1u;
    }
    let weighted = abs((x_coverage * x_weight + y_coverage * y_weight) /
        max(x_weight + y_weight, COVERAGE_EPSILON));
    return clamp(max(weighted, min(abs(x_coverage), abs(y_coverage))), 0.0, 1.0);
}

fn glyph_coverage(glyph_base: u32, sample: vec2<f32>, scale: f32) -> f32 {
    let step = 0.25 / max(scale, COVERAGE_EPSILON);
    return (
        coverage_single(glyph_base, sample + vec2<f32>(-step, -step), scale) +
        coverage_single(glyph_base, sample + vec2<f32>( step, -step), scale) +
        coverage_single(glyph_base, sample + vec2<f32>(-step,  step), scale) +
        coverage_single(glyph_base, sample + vec2<f32>( step,  step), scale)
    ) * 0.25;
}

@compute @workgroup_size(8, 8, 1)
fn gpu_slug_main(@builtin(global_invocation_id) invocation: vec3<u32>) {
    let region_count = frame.words[10u];
    if (invocation.z >= region_count) {
        return;
    }
    let region_base = invocation.z * 4u;
    let region_origin = vec2<u32>(regions.words[region_base], regions.words[region_base + 1u]);
    let region_extent = vec2<u32>(regions.words[region_base + 2u], regions.words[region_base + 3u]);
    if (invocation.x >= region_extent.x || invocation.y >= region_extent.y) {
        return;
    }
    let pixel = region_origin + invocation.xy;
    let target_extent = vec2<u32>(frame.words[0u], frame.words[1u]);
    if (pixel.x >= target_extent.x || pixel.y >= target_extent.y) {
        return;
    }

    let columns = frame.words[2u];
    let rows = frame.words[3u];
    let cell_width = frame.words[4u];
    let cell_height = frame.words[5u];
    let cell_position = vec2<u32>(pixel.x / cell_width, pixel.y / cell_height);
    var output = unpack_rgba(frame.words[15u]);
    if (cell_position.x < columns && cell_position.y < rows) {
        let cell_index = cell_position.y * columns + cell_position.x;
        let cell_base = cell_index * CELL_WORDS;
        let glyph_index = cells.words[cell_base];
        let foreground = unpack_rgba(cells.words[cell_base + 1u]);
        let background = unpack_rgba(cells.words[cell_base + 2u]);
        let flags = cells.words[cell_base + 3u];
        let origin_column = cells.words[cell_base + 4u];
        let glyph_cell_width = max(cells.words[cell_base + 5u], 1u);
        output = background;

        let glyph_base = glyph_index * GLYPH_WORDS;
        let units_per_em = f32(max(glyphs.words[glyph_base + 6u], 1u));
        let requested_pixel_size = max(bitcast<f32>(frame.words[14u]), 1.0);
        let scale = requested_pixel_size / units_per_em;
        let advance = bitcast<f32>(glyphs.words[glyph_base + 7u]);
        let glyph_span_pixels = f32(cell_width * glyph_cell_width);
        let centered_offset = max((glyph_span_pixels - advance * scale) * 0.5, 0.0);
        let glyph_origin_x = f32(origin_column * cell_width) + centered_offset;
        let ascender = bitcast<f32>(frame.words[12u]);
        let sample = vec2<f32>(
            (f32(pixel.x) + 0.5 - glyph_origin_x) / scale,
            ascender - (f32(pixel.y % cell_height) + 0.5) / scale,
        );
        let coverage = glyph_coverage(glyph_base, sample, scale);
        output = mix(background, foreground, coverage);

        if ((flags & 1u) != 0u && pixel.y % cell_height == cell_height - 2u) {
            output = foreground;
        }
        if (frame.words[8u] != 0u && cell_position.x == frame.words[6u] &&
            cell_position.y == frame.words[7u] && pixel.x % cell_width < 2u) {
            output = vec4<f32>(210.0, 210.0, 210.0, 190.0) / 255.0;
        }
    }
    textureStore(output_image, vec2<i32>(pixel), output);
}
