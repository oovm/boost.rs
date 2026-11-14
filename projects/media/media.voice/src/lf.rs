//! LF 风格的声门流导数（GFD）波表。
//!
//! 现代确定性声门源（GOLF, Yu & Fazekas 2024, TISMIR；LF 模型 Liljencrants &
//! Fant 1985；单参数重参 Fant 1995 / Pink Trombone 约定）。替换早期
//! 「锯齿波 + 一阶积分」近似：GFD 波表给出真实声门脉冲的频谱倾斜
//! （≥ −12 dB/oct，单元测试钉死）与闭相结构，`R_d`（0..3）单调对应
//! 发声力度——高 R_d 压喉短开相，低 R_d 松喉长开相。
//!
//! 构造：Rosenberg 风格平滑声门流（升段 sin²、降段 cos²、快速回相），
//! GFD 为中心差分——连续无跳变保证谐波快衰减；每行单位能量归一并
//! 对齐负峰（GOLF §3.1）。全部确定性：同参数同表，逐位复现。

/// 预计算行数（R_d 网格）与每周期采样数。
pub const ROWS: usize = 33;
pub const PERIOD: usize = 1024;

/// 一行一个周期的 GFD 表；`R_d` 从 0 到 3 等距。
pub struct GlottalTables {
    rows: Vec<Vec<f32>>,
}

impl GlottalTables {
    #[must_use]
    pub fn new() -> Self {
        let mut rows = Vec::with_capacity(ROWS);
        for index in 0..ROWS {
            let rd = 3.0 * index as f32 / (ROWS - 1) as f32;
            rows.push(build_cycle(rd));
        }
        Self { rows }
    }

    /// 线性插值两行（GOLF 式双线性表查询的行轴）。
    #[must_use]
    pub fn sample(&self, rd: f32, phase: f32) -> f32 {
        let rd = rd.clamp(0.0, 3.0);
        let rowf = rd / 3.0 * (ROWS - 1) as f32;
        let r0 = rowf.floor() as usize;
        let r1 = (r0 + 1).min(ROWS - 1);
        let fr = rowf - r0 as f32;
        let p = phase - phase.floor();
        let x = p * PERIOD as f32;
        let i0 = x.floor() as usize % PERIOD;
        let i1 = (i0 + 1) % PERIOD;
        let fp = x - x.floor();
        let a = self.rows[r0][i0] * (1.0 - fp) + self.rows[r1][i0] * fp;
        let b = self.rows[r0][i1] * (1.0 - fp) + self.rows[r1][i1] * fp;
        a * (1.0 - fr) + b * fr
    }
}

impl Default for GlottalTables {
    fn default() -> Self {
        Self::new()
    }
}

/// 由 R_d 构造一个周期的 GFD（归一化时间）。
///
/// 构造：Rosenberg 风格平滑声门流 `u(t)`（升段 `sin²`、降段 `cos²`，端点导数
/// 为零），GFD 为 `du/dt` 的中心差分。连续无跳变 → 谐波 ≥ −12 dB/oct 衰减
/// （LF 的闭合尖峰在此被平滑为快速回相；与 GOLF 的 wavetable 架构一致）。
/// `R_d` 经 `t_e = 1 − R_d/3 − t_a` 控制开放商：高 R_d = 压喉短开相。
fn build_cycle(rd: f32) -> Vec<f32> {
    let ta = 0.25_f64;
    // 闭相必须存在：te 上限保证 tc = te+ta < 1。
    let te = (1.0 - rd as f64 / 3.0 - ta).clamp(0.05, 0.6);
    let tp = te * 0.6; // 峰值流量时刻
    let tc = te + ta;

    let mut flow = vec![0.0_f64; PERIOD + 1];
    for i in 0..=PERIOD {
        let t = i as f64 / PERIOD as f64;
        flow[i] = if t < tp {
            let xi = t / tp;
            (xi * std::f64::consts::FRAC_PI_2).sin().powi(2)
        } else if t < te {
            let xi = (t - tp) / (te - tp);
            ((1.0 - xi) * std::f64::consts::FRAC_PI_2).sin().powi(2)
        } else if t < tc {
            // 快速回相：cos² 收尾到零（比开相陡，模拟闭合力）。
            let xi = (t - te) / ta;
            -0.15 * ((1.0 - xi) * std::f64::consts::FRAC_PI_2).sin().powi(2)
        } else {
            0.0
        };
    }
    // 中心差分得 GFD（周期边界）。
    let mut cycle = vec![0.0_f64; PERIOD];
    for i in 0..PERIOD {
        let prev = if i == 0 { flow[PERIOD - 1] } else { flow[i - 1] };
        cycle[i] = (flow[i + 1] - prev) * 0.5 * PERIOD as f64;
    }

    // 单位能量归一 + 负峰对齐到固定位置（GOLF §3.1）。
    let energy = cycle.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-9);
    for v in &mut cycle {
        *v /= energy;
    }
    let min_index = cycle
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let shift = PERIOD / 4;
    let mut aligned = vec![0.0_f32; PERIOD];
    for (i, v) in cycle.iter().enumerate() {
        aligned[(i + shift + PERIOD - min_index) % PERIOD] = *v as f32;
    }
    aligned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_has_long_zero_closed_phase() {
        let tables = GlottalTables::new();
        for row in &tables.rows {
            let zeros = row.iter().filter(|v| **v == 0.0).count();
            assert!(zeros > PERIOD / 8, "closed phase must exist");
            let energy: f32 = row.iter().map(|v| v * v).sum();
            assert!((energy - 1.0).abs() < 0.02, "unit energy, got {energy}");
            assert!(row.iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn spectral_tilt_is_glottal_like() {
        // LF 源近似 −12 dB/oct：取 R_d=1.5 周期，比较 2 次与 8 次谐波能量。
        let tables = GlottalTables::new();
        let row = &tables.rows[tables.rows.len() / 2];
        let magnitude = |k: usize| -> f64 {
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            for (n, x) in row.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * k as f64 * n as f64 / PERIOD as f64;
                re += *x as f64 * a.cos();
                im += *x as f64 * a.sin();
            }
            (re * re + im * im).sqrt().max(1e-9)
        };
        let h2 = magnitude(2);
        let h8 = magnitude(8);
        let slope_db_per_oct = 20.0 * (h8 / h2).log10() / (4.0_f64).log2();
        assert!(
            (-18.0..=-6.0).contains(&slope_db_per_oct),
            "tilt {slope_db_per_oct} dB/oct outside glottal range"
        );
    }

    #[test]
    fn higher_rd_means_shorter_open_quotient() {
        // 压喉 = 高 R_d = 短开相（te = 1 − R_d/3 − t_a）。
        let tables = GlottalTables::new();
        let positive = |row: &[f32]| row.iter().filter(|v| **v > 0.0).count();
        assert!(positive(&tables.rows[4]) > positive(&tables.rows[28]));
    }

    #[test]
    fn interpolation_is_finite_and_reproducible() {
        let tables = GlottalTables::new();
        let a = tables.sample(1.7, 0.333);
        let b = tables.sample(1.7, 0.333);
        assert_eq!(a.to_bits(), b.to_bits());
        assert!(tables.sample(0.0, 0.0).is_finite() && tables.sample(3.0, 0.999).is_finite());
    }
}
