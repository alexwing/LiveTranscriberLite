//! Remuestreo por bloques a la frecuencia del modelo.
//!
//! En Windows no hace falta: WASAPI con `autoconvert` ya entrega 16 kHz mono.
//! Core Audio no convierte por nosotros, asi que en macOS el audio llega a la
//! frecuencia del dispositivo (44,1 o 48 kHz) y hay que bajarlo aqui.
//!
//! Para voz basta algo sencillo y estable: un paso bajo Butterworth de cuarto
//! orden (dos biquads) justo por debajo del Nyquist de salida, para que lo que
//! esta por encima de 8 kHz no se pliegue sobre la voz, y luego interpolacion
//! lineal en la posicion fraccionaria. Funciona por bloques de cualquier
//! tamano y da lo mismo que procesar todo de una vez.

/// Filtro biquad en forma directa II transpuesta.
#[derive(Debug, Clone)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// Paso bajo de la "Audio EQ Cookbook" (RBJ).
    fn lowpass(sample_rate: f64, cutoff: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * cutoff / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cos) / 2.0 / a0,
            b1: (1.0 - cos) / a0,
            b2: (1.0 - cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Convierte un flujo mono de `input_rate` a `output_rate`, por bloques.
#[derive(Debug, Clone)]
pub struct Resampler {
    /// Muestras de entrada que avanza cada muestra de salida.
    step: f64,
    /// Posicion de la siguiente muestra de salida, relativa a `buf[0]`.
    pos: f64,
    /// Ultima muestra filtrada del bloque anterior: la interpolacion la
    /// necesita para el tramo que cruza de un bloque al siguiente.
    carry: Option<f32>,
    /// Sin filtro si no se baja la frecuencia (no hay nada que plegar).
    filters: Vec<Biquad>,
    buf: Vec<f32>,
}

impl Resampler {
    pub fn new(input_rate: f64, output_rate: f64) -> Self {
        let filters = if input_rate > output_rate * 1.001 {
            // Butterworth de 4.o orden: dos secciones con estas Q.
            let cutoff = 0.45 * output_rate;
            vec![
                Biquad::lowpass(input_rate, cutoff, 0.541_196),
                Biquad::lowpass(input_rate, cutoff, 1.306_563),
            ]
        } else {
            Vec::new()
        };
        Self {
            step: input_rate / output_rate,
            pos: 0.0,
            carry: None,
            filters,
            buf: Vec::new(),
        }
    }

    /// Anade a `out` las muestras de salida que ya se pueden calcular con
    /// `input`. Lo que queda a medias se guarda para el siguiente bloque.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.clear();
        if let Some(carry) = self.carry {
            self.buf.push(carry);
        }
        for &x in input {
            let mut y = x as f64;
            for f in &mut self.filters {
                y = f.run(y);
            }
            self.buf.push(y as f32);
        }
        let Some(&last) = self.buf.last() else {
            return;
        };
        let len = self.buf.len() as f64;
        while self.pos + 1.0 < len {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let (a, b) = (self.buf[i], self.buf[i + 1]);
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        // La ultima muestra pasa a ser `buf[0]` del siguiente bloque.
        self.pos -= len - 1.0;
        self.carry = Some(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: f64, freq: f64, amp: f32, secs: f64) -> Vec<f32> {
        let n = (rate * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Frecuencia aproximada por cruces por cero ascendentes.
    fn freq(x: &[f32], rate: f64) -> f64 {
        let ups = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f64 * rate / x.len() as f64
    }

    fn all(input_rate: f64, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(input_rate, 16_000.0);
        let mut out = Vec::new();
        r.process(input, &mut out);
        out
    }

    #[test]
    fn de_48k_a_16k_conserva_frecuencia_y_nivel() {
        let out = all(48_000.0, &sine(48_000.0, 1_000.0, 0.5, 1.0));
        assert!((out.len() as i64 - 16_000).abs() <= 2, "len {}", out.len());
        // Fuera el arranque del filtro.
        let tail = &out[800..];
        assert!((freq(tail, 16_000.0) - 1_000.0).abs() < 10.0);
        assert!((rms(tail) - 0.3536).abs() < 0.03, "rms {}", rms(tail));
    }

    #[test]
    fn de_44k1_a_16k_da_la_longitud_correcta() {
        let out = all(44_100.0, &sine(44_100.0, 440.0, 0.5, 2.0));
        assert!((out.len() as i64 - 32_000).abs() <= 2, "len {}", out.len());
        assert!((freq(&out[800..], 16_000.0) - 440.0).abs() < 5.0);
    }

    #[test]
    fn lo_que_pasa_de_8k_no_se_pliega_sobre_la_voz() {
        let out = all(48_000.0, &sine(48_000.0, 12_000.0, 0.5, 1.0));
        assert!(rms(&out[800..]) < 0.03, "rms {}", rms(&out[800..]));
    }

    #[test]
    fn por_bloques_da_lo_mismo_que_de_una_vez() {
        let input = sine(48_000.0, 700.0, 0.4, 0.5);
        let whole = all(48_000.0, &input);
        let mut r = Resampler::new(48_000.0, 16_000.0);
        let mut chunked = Vec::new();
        for block in input.chunks(441) {
            r.process(block, &mut chunked);
        }
        assert_eq!(whole.len(), chunked.len());
        for (a, b) in whole.iter().zip(&chunked) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn a_la_misma_frecuencia_no_cambia_nada() {
        let input = sine(16_000.0, 300.0, 0.5, 0.1);
        let out = all(16_000.0, &input);
        // Se queda una muestra pendiente para el siguiente bloque.
        assert_eq!(out.len(), input.len() - 1);
        for (a, b) in out.iter().zip(&input) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
