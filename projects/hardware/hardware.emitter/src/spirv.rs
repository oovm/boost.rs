use crate::spv::{self, Spv, builtin, decoration, execution_mode, execution_model, inst, op, storage};
use hardware_types::{ShaderOp, ShaderProgram};

/// SPIR-V module for the Raster triangle thin gate (vertex + fragment entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterTriSpirv {
    /// Module bytes containing `vs_main` and `fs_main`.
    pub module: Vec<u8>,
}

/// Emit SPIR-V words (as little-endian bytes) for the given UCF program.
///
/// The built-in kernels are assembled directly into SPIR-V words. Authors never
/// write shaders and no external shader compiler is involved.
pub fn emit_spirv(program: &ShaderProgram) -> Result<Vec<u8>, String> {
    for op in &program.ops {
        match op {
            ShaderOp::MemCopy => continue,
            ShaderOp::IotaFill { value } => {
                return Ok(fill_module(&program.entry, *value));
            }
            ShaderOp::MatMul => {
                return Ok(matmul_module(&program.entry));
            }
            ShaderOp::Rgba8Denoise => {
                return Err("RGBA8 denoise is CUDA-only in this thin gate".into());
            }
            ShaderOp::Attention => {
                return Err("Attention HSACO/PTX path is not emitted as SPIR-V".into());
            }
        }
    }
    Err("program has no emittable SPIR-V ops".into())
}

/// Emit a hard-coded NDC yellow triangle (VS+FS) for Raster `draw=tri`.
pub fn emit_raster_tri_spirv() -> Result<RasterTriSpirv, String> {
    let mut spv = Spv::new();
    let u32_ty = spv.u32_;
    let bool_ty = spv.bool_;
    let vec2_ty = spv.vec2_f32;
    let vec4_ty = spv.vec4_f32;

    let out_vec4_ptr = spv.pointer(storage::OUTPUT, vec4_ty);
    let in_u32_ptr = spv.pointer(storage::INPUT, u32_ty);
    let in_vec4_ptr = spv.pointer(storage::INPUT, vec4_ty);

    let out_pos = spv.global(out_vec4_ptr, storage::OUTPUT);
    let out_col = spv.global(out_vec4_ptr, storage::OUTPUT);
    spv.decorate(out_pos, decoration::BUILT_IN, &[builtin::POSITION]);
    spv.decorate(out_col, decoration::LOCATION, &[0]);
    let in_vid = spv.global(in_u32_ptr, storage::INPUT);
    spv.decorate(in_vid, decoration::BUILT_IN, &[builtin::VERTEX_INDEX]);
    let fs_in_col = spv.global(in_vec4_ptr, storage::INPUT);
    spv.decorate(fs_in_col, decoration::LOCATION, &[0]);
    let fs_out_color = spv.global(out_vec4_ptr, storage::OUTPUT);
    spv.decorate(fs_out_color, decoration::LOCATION, &[0]);

    let zero = spv.const_u32(0);
    let one = spv.const_u32(1);
    let f_zero = spv.const_f32(0.0);
    let f_one = spv.const_f32(1.0);
    let f_low = spv.const_f32(-0.8);
    let f_high = spv.const_f32(0.8);
    let v0 = spv.const_composite(vec2_ty, &[f_low, f_low]);
    let v1 = spv.const_composite(vec2_ty, &[f_high, f_low]);
    let v2 = spv.const_composite(vec2_ty, &[f_zero, f_high]);
    let color = spv.const_composite(vec4_ty, &[f_one, f_one, f_zero, f_one]);

    // Vertex entry: `verts[vid]` expands into two selects over three constants.
    let vs_ty = spv.function_type(spv.void, &[]);
    let vs_id = spv.fresh();
    let mut vs = Vec::new();
    inst(&mut vs, op::FUNCTION, &[spv.void, vs_id, 0, vs_ty]);
    let vs_label = spv.fresh();
    inst(&mut vs, op::LABEL, &[vs_label]);

    let vid = spv::load(&mut spv, &mut vs, u32_ty, in_vid);
    let is_zero = spv::binary(&mut spv, &mut vs, bool_ty, op::I_EQUAL, vid, zero);
    let is_one = spv::binary(&mut spv, &mut vs, bool_ty, op::I_EQUAL, vid, one);
    let pick_1_2 = spv::select(&mut spv, &mut vs, vec2_ty, is_one, v1, v2);
    let corner = spv::select(&mut spv, &mut vs, vec2_ty, is_zero, v0, pick_1_2);
    let position = spv::construct(&mut spv, &mut vs, vec4_ty, &[corner, f_zero, f_one]);
    spv::store(&mut vs, out_pos, position);
    spv::store(&mut vs, out_col, color);
    inst(&mut vs, op::RETURN, &[]);
    inst(&mut vs, op::FUNCTION_END, &[]);

    // Fragment entry: pass the interpolated colour through.
    let fs_ty = spv.function_type(spv.void, &[]);
    let fs_id = spv.fresh();
    let mut fs = Vec::new();
    inst(&mut fs, op::FUNCTION, &[spv.void, fs_id, 0, fs_ty]);
    let fs_label = spv.fresh();
    inst(&mut fs, op::LABEL, &[fs_label]);
    let loaded = spv::load(&mut spv, &mut fs, vec4_ty, fs_in_col);
    spv::store(&mut fs, fs_out_color, loaded);
    inst(&mut fs, op::RETURN, &[]);
    inst(&mut fs, op::FUNCTION_END, &[]);

    spv.entry_point(execution_model::VERTEX, vs_id, "vs_main", &[out_pos, out_col, in_vid]);
    spv.entry_point(execution_model::FRAGMENT, fs_id, "fs_main", &[fs_in_col, fs_out_color]);
    spv.execution_mode(fs_id, execution_mode::ORIGIN_UPPER_LEFT, &[]);
    spv.push_function(vs);
    spv.push_function(fs);
    Ok(RasterTriSpirv { module: spv.finish() })
}

/// `out[i] = value for i in 0..count`.
fn fill_module(entry: &str, value: f32) -> Vec<u8> {
    let mut spv = Spv::new();
    let u32_ty = spv.u32_;
    let bool_ty = spv.bool_;
    let f32_ty = spv.f32_;
    let gid_ty = spv.vec3_u32;

    let push_struct = spv.block_struct(&[(u32_ty, 0)]);
    let push_ptr = spv.pointer(storage::PUSH_CONSTANT, push_struct);
    let push_u32_ptr = spv.pointer(storage::PUSH_CONSTANT, u32_ty);
    let push_var = spv.global(push_ptr, storage::PUSH_CONSTANT);

    let out_array = spv.runtime_array(f32_ty, 4);
    let out_struct = spv.block_struct(&[(out_array, 0)]);
    let out_ptr = spv.pointer(storage::STORAGE_BUFFER, out_struct);
    let out_f32_ptr = spv.pointer(storage::STORAGE_BUFFER, f32_ty);
    let out_var = spv.global(out_ptr, storage::STORAGE_BUFFER);
    spv.decorate(out_var, decoration::DESCRIPTOR_SET, &[0]);
    spv.decorate(out_var, decoration::BINDING, &[0]);

    let gid_ptr = spv.pointer(storage::INPUT, gid_ty);
    let gid_var = spv.global(gid_ptr, storage::INPUT);
    spv.decorate(gid_var, decoration::BUILT_IN, &[builtin::GLOBAL_INVOCATION_ID]);

    let function_ty = spv.function_type(spv.void, &[]);
    let function = spv.fresh();
    let mut words = Vec::new();
    inst(&mut words, op::FUNCTION, &[spv.void, function, 0, function_ty]);
    let label = spv.fresh();
    inst(&mut words, op::LABEL, &[label]);

    let zero = spv.const_u32(0);
    let value_id = spv.const_f32(value);
    let mut body = Vec::new();
    let gid = spv::load(&mut spv, &mut body, gid_ty, gid_var);
    let gid_x = spv::extract(&mut spv, &mut body, u32_ty, gid, 0);
    let count_ptr = spv::chain(&mut spv, &mut body, push_u32_ptr, push_var, &[zero]);
    let count = spv::load(&mut spv, &mut body, u32_ty, count_ptr);
    let in_bounds = spv::binary(&mut spv, &mut body, bool_ty, op::U_LESS_THAN, gid_x, count);

    let active = spv.fresh();
    let inactive = spv.fresh();
    let merge = spv.fresh();
    inst(&mut body, op::SELECTION_MERGE, &[merge, 0]);
    inst(&mut body, op::BRANCH_CONDITIONAL, &[in_bounds, active, inactive]);
    inst(&mut body, op::LABEL, &[active]);
    let element = spv::chain(&mut spv, &mut body, out_f32_ptr, out_var, &[zero, gid_x]);
    spv::store(&mut body, element, value_id);
    inst(&mut body, op::BRANCH, &[merge]);
    inst(&mut body, op::LABEL, &[inactive]);
    inst(&mut body, op::BRANCH, &[merge]);
    inst(&mut body, op::LABEL, &[merge]);
    inst(&mut body, op::RETURN, &[]);

    words.append(&mut body);
    inst(&mut words, op::FUNCTION_END, &[]);
    spv.push_function(words);

    spv.entry_point(execution_model::GL_COMPUTE, function, entry, &[gid_var, push_var, out_var]);
    spv.execution_mode(function, execution_mode::LOCAL_SIZE, &[64, 1, 1]);
    spv.finish()
}

/// Naive row-major matmul: `C[m,n] = A[m,k] * B[k,n]`.
fn matmul_module(entry: &str) -> Vec<u8> {
    let mut spv = Spv::new();
    let u32_ty = spv.u32_;
    let bool_ty = spv.bool_;
    let f32_ty = spv.f32_;
    let gid_ty = spv.vec3_u32;
    let ptr_fn_u32 = spv.pointer(storage::FUNCTION, u32_ty);
    let ptr_fn_f32 = spv.pointer(storage::FUNCTION, f32_ty);

    let push_struct = spv.block_struct(&[(u32_ty, 0), (u32_ty, 4), (u32_ty, 8), (u32_ty, 12)]);
    let push_ptr = spv.pointer(storage::PUSH_CONSTANT, push_struct);
    let push_u32_ptr = spv.pointer(storage::PUSH_CONSTANT, u32_ty);
    let push_var = spv.global(push_ptr, storage::PUSH_CONSTANT);

    let buffer_type = |spv: &mut Spv| {
        let array = spv.runtime_array(f32_ty, 4);
        let structure = spv.block_struct(&[(array, 0)]);
        spv.pointer(storage::STORAGE_BUFFER, structure)
    };
    let element_ptr = spv.pointer(storage::STORAGE_BUFFER, f32_ty);
    let a_ptr = buffer_type(&mut spv);
    let b_ptr = buffer_type(&mut spv);
    let out_ptr = buffer_type(&mut spv);
    let a_var = spv.global(a_ptr, storage::STORAGE_BUFFER);
    let b_var = spv.global(b_ptr, storage::STORAGE_BUFFER);
    let out_var = spv.global(out_ptr, storage::STORAGE_BUFFER);
    spv.decorate(a_var, decoration::DESCRIPTOR_SET, &[0]);
    spv.decorate(a_var, decoration::BINDING, &[0]);
    spv.decorate(b_var, decoration::DESCRIPTOR_SET, &[0]);
    spv.decorate(b_var, decoration::BINDING, &[1]);
    spv.decorate(out_var, decoration::DESCRIPTOR_SET, &[0]);
    spv.decorate(out_var, decoration::BINDING, &[2]);

    let gid_ptr = spv.pointer(storage::INPUT, gid_ty);
    let gid_var = spv.global(gid_ptr, storage::INPUT);
    spv.decorate(gid_var, decoration::BUILT_IN, &[builtin::GLOBAL_INVOCATION_ID]);

    let function_ty = spv.function_type(spv.void, &[]);
    let function = spv.fresh();
    let mut words = Vec::new();
    inst(&mut words, op::FUNCTION, &[spv.void, function, 0, function_ty]);
    let label = spv.fresh();
    inst(&mut words, op::LABEL, &[label]);

    let mut locals = Vec::new();
    let iter_var = spv.fresh();
    inst(&mut locals, op::VARIABLE, &[ptr_fn_u32, iter_var, storage::FUNCTION]);
    let acc_var = spv.fresh();
    inst(&mut locals, op::VARIABLE, &[ptr_fn_f32, acc_var, storage::FUNCTION]);

    let zero = spv.const_u32(0);
    let one = spv.const_u32(1);
    let two = spv.const_u32(2);
    let f_zero = spv.const_f32(0.0);

    let mut body = Vec::new();
    spv::store(&mut body, iter_var, zero);
    spv::store(&mut body, acc_var, f_zero);
    let gid = spv::load(&mut spv, &mut body, gid_ty, gid_var);
    let idx = spv::extract(&mut spv, &mut body, u32_ty, gid, 0);
    let m_ptr = spv::chain(&mut spv, &mut body, push_u32_ptr, push_var, &[zero]);
    let m = spv::load(&mut spv, &mut body, u32_ty, m_ptr);
    let n_ptr = spv::chain(&mut spv, &mut body, push_u32_ptr, push_var, &[one]);
    let n = spv::load(&mut spv, &mut body, u32_ty, n_ptr);
    let k_ptr = spv::chain(&mut spv, &mut body, push_u32_ptr, push_var, &[two]);
    let k = spv::load(&mut spv, &mut body, u32_ty, k_ptr);
    let total = spv::binary(&mut spv, &mut body, u32_ty, op::I_MUL, m, n);
    let out_of_bounds = spv::binary(&mut spv, &mut body, bool_ty, op::U_GREATER_THAN_EQUAL, idx, total);

    let row_block = spv.fresh();
    let exit_block = spv.fresh();
    let entry_merge = spv.fresh();
    let loop_block = spv.fresh();
    let body_block = spv.fresh();
    let continue_block = spv.fresh();
    let merge_block = spv.fresh();

    inst(&mut body, op::SELECTION_MERGE, &[entry_merge, 0]);
    inst(&mut body, op::BRANCH_CONDITIONAL, &[out_of_bounds, exit_block, row_block]);
    inst(&mut body, op::LABEL, &[row_block]);
    let row = spv::binary(&mut spv, &mut body, u32_ty, op::U_DIV, idx, n);
    let col = spv::binary(&mut spv, &mut body, u32_ty, op::U_MOD, idx, n);
    inst(&mut body, op::BRANCH, &[entry_merge]);

    inst(&mut body, op::LABEL, &[exit_block]);
    inst(&mut body, op::RETURN, &[]);

    inst(&mut body, op::LABEL, &[entry_merge]);
    inst(&mut body, op::BRANCH, &[loop_block]);

    inst(&mut body, op::LABEL, &[loop_block]);
    inst(&mut body, op::LOOP_MERGE, &[merge_block, continue_block, 0]);
    let iter = spv::load(&mut spv, &mut body, u32_ty, iter_var);
    let done = spv::binary(&mut spv, &mut body, bool_ty, op::U_GREATER_THAN_EQUAL, iter, k);
    inst(&mut body, op::BRANCH_CONDITIONAL, &[done, merge_block, body_block]);

    inst(&mut body, op::LABEL, &[body_block]);
    let row_offset = spv::binary(&mut spv, &mut body, u32_ty, op::I_MUL, row, k);
    let a_index = spv::binary(&mut spv, &mut body, u32_ty, op::I_ADD, row_offset, iter);
    let a_element = spv::chain(&mut spv, &mut body, element_ptr, a_var, &[zero, a_index]);
    let a_value = spv::load(&mut spv, &mut body, f32_ty, a_element);
    let column_offset = spv::binary(&mut spv, &mut body, u32_ty, op::I_MUL, iter, n);
    let b_index = spv::binary(&mut spv, &mut body, u32_ty, op::I_ADD, column_offset, col);
    let b_element = spv::chain(&mut spv, &mut body, element_ptr, b_var, &[zero, b_index]);
    let b_value = spv::load(&mut spv, &mut body, f32_ty, b_element);
    let product = spv::binary(&mut spv, &mut body, f32_ty, op::F_MUL, a_value, b_value);
    let acc = spv::load(&mut spv, &mut body, f32_ty, acc_var);
    let sum = spv::binary(&mut spv, &mut body, f32_ty, op::F_ADD, acc, product);
    spv::store(&mut body, acc_var, sum);
    inst(&mut body, op::BRANCH, &[continue_block]);

    inst(&mut body, op::LABEL, &[continue_block]);
    let current = spv::load(&mut spv, &mut body, u32_ty, iter_var);
    let next = spv::binary(&mut spv, &mut body, u32_ty, op::I_ADD, current, one);
    spv::store(&mut body, iter_var, next);
    inst(&mut body, op::BRANCH, &[loop_block]);

    inst(&mut body, op::LABEL, &[merge_block]);
    let out_element = spv::chain(&mut spv, &mut body, element_ptr, out_var, &[zero, idx]);
    let accumulated = spv::load(&mut spv, &mut body, f32_ty, acc_var);
    spv::store(&mut body, out_element, accumulated);
    inst(&mut body, op::RETURN, &[]);

    words.extend_from_slice(&locals);
    words.append(&mut body);
    inst(&mut words, op::FUNCTION_END, &[]);
    spv.push_function(words);

    spv.entry_point(execution_model::GL_COMPUTE, function, entry, &[gid_var, push_var, a_var, b_var, out_var]);
    spv.execution_mode(function, execution_mode::LOCAL_SIZE, &[64, 1, 1]);
    spv.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardware_types::ShaderProgram;

    #[test]
    fn emit_fill_and_matmul_spirv() {
        let fill = emit_spirv(&ShaderProgram::dispatch_fill("ucf_fill", 1.0)).expect("fill");
        assert!(fill.len() > 20);
        assert_eq!(&fill[0..4], &[0x03, 0x02, 0x23, 0x07]); // SPIR-V magic LE
        let matmul = emit_spirv(&ShaderProgram::matmul("ucf_matmul")).expect("matmul");
        assert!(matmul.len() > 20);
        assert_eq!(&matmul[0..4], &[0x03, 0x02, 0x23, 0x07]);
    }

    #[test]
    fn emit_raster_tri_spirv_module() {
        let tri = emit_raster_tri_spirv().expect("tri");
        assert!(tri.module.len() > 20);
        assert_eq!(&tri.module[0..4], &[0x03, 0x02, 0x23, 0x07]);
    }
}
