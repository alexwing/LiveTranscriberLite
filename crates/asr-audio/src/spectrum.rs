//! Espectro de bandas para el ecualizador de la interfaz.
//!
//! Port simplificado del analizador de TapoController (`disco.rs`) para 16 kHz
//! mono. Cada bloque de audio se añade a un historial de [`FFT_SIZE`] muestras
//! y se analiza entero: asi la ventana de FFT no depende del tamaño del bloque
//! (100 ms en la captura, pero cualquier longitud sirve).
//!
//! Las bandas son log-espaciadas de 60 Hz a 7,5 kHz, con una pendiente de
//! +3 dB por octava para que una mezcla tipica (rosada) se vea mas o menos
//! plana. La normalizacion sigue el mismo criterio que `AutoRef` de
//! TapoController: una referencia de nivel que sube al instante y baja despacio,
//! para que el ecualizador se autoajuste al volumen del sistema.

use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

use crate::TARGET_RATE;

/// Numero de bandas que devuelve [`Spectrum::analyze`].
pub const BANDS: usize = 16;

/// Muestras por ventana de FFT (64 ms a 16 kHz).
const FFT_SIZE: usize = 1024;

const LOW_HZ: f32 = 60.0;
const HIGH_HZ: f32 = 7_500.0;

/// Cuanto baja la referencia en cada llamada: ~6 dB/s a 10 llamadas/s.
const DECAY_DB: f32 = 0.6;

/// La referencia no baja de aqui, y las bandas se miden hasta 45 dB por debajo.
const FLOOR_DB: f32 = -60.0;
const RANGE_DB: f32 = 45.0;

/// Por debajo de esto (-80 dBFS de media cuadratica) el historial es silencio
/// digital y todas las bandas valen 0. Sin esta puerta, la banda mas aguda de
/// un silencio absoluto (tilt de +18 dB, potencia 1e-12) quedaria en ~0,07.
const SILENCE_MS: f32 = 1e-8;

/// Analizador de espectro con su plan de FFT y su referencia de nivel.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex32>,
    /// Ultimas `FFT_SIZE` muestras, la mas antigua primero. Empieza a ceros.
    history: Vec<f32>,
    /// Por banda: rango de bins `[from, to)` y la pendiente en dB.
    band_bins: Vec<(usize, usize, f32)>,
    /// Maximo de las bandas con caida lenta. Ver [`Spectrum::analyze`].
    reference_db: f32,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

/// Bin de la FFT mas cercano a una frecuencia, sin salirse de la mitad util.
fn bin_of(freq: f32) -> usize {
    ((freq * FFT_SIZE as f32 / TARGET_RATE as f32).round() as usize).clamp(1, FFT_SIZE / 2 - 1)
}

fn to_db(power: f32) -> f32 {
    10.0 * (power + 1e-12).log10()
}

impl Spectrum {
    pub fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT_SIZE - 1) as f32).cos()
            })
            .collect();

        let mut band_bins = Vec::with_capacity(BANDS);
        for i in 0..BANDS {
            let f0 = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(i as f32 / BANDS as f32);
            let f1 = LOW_HZ * (HIGH_HZ / LOW_HZ).powf((i + 1) as f32 / BANDS as f32);
            let a = bin_of(f0);
            let b = bin_of(f1).max(a + 1);
            let center = (f0 * f1).sqrt();
            let tilt = 3.0 * (center / 100.0).log2();
            band_bins.push((a, b, tilt));
        }

        Self {
            fft,
            window,
            buf: vec![Complex32::new(0.0, 0.0); FFT_SIZE],
            history: vec![0.0; FFT_SIZE],
            band_bins,
            reference_db: FLOOR_DB,
        }
    }

    /// Analiza un bloque de audio f32 mono a 16 kHz y devuelve [`BANDS`] valores
    /// en 0..=1, de graves a agudos. Un bloque de cualquier longitud vale: se
    /// añade al historial y se analiza la ventana entera.
    pub fn analyze(&mut self, block: &[f32]) -> Vec<f32> {
        self.push_history(block);

        for (slot, (&s, &w)) in self
            .buf
            .iter_mut()
            .zip(self.history.iter().zip(self.window.iter()))
        {
            *slot = Complex32::new(s * w, 0.0);
        }
        self.fft.process(&mut self.buf);

        // Potencia por bin, normalizada para que un seno de amplitud 1 de ~0 dB.
        let scale = 2.0 / (FFT_SIZE as f32 * 0.5);
        let power: Vec<f32> = self.buf[..FFT_SIZE / 2]
            .iter()
            .map(|c| {
                let m = c.norm() * scale;
                m * m
            })
            .collect();

        // Potencia media de cada banda, en dB con su pendiente.
        let band_db: Vec<f32> = self
            .band_bins
            .iter()
            .map(|&(a, b, tilt)| {
                let p = power[a..b].iter().sum::<f32>() / (b - a) as f32;
                to_db(p) + tilt
            })
            .collect();

        // La referencia sube al instante con la banda mas fuerte y baja despacio.
        let max_db = band_db.iter().cloned().fold(f32::MIN, f32::max);
        self.reference_db = (self.reference_db - DECAY_DB).max(max_db).max(FLOOR_DB);
        let floor = self.reference_db - RANGE_DB;

        if self.is_silent() {
            return vec![0.0; BANDS];
        }
        band_db
            .iter()
            .map(|&db| ((db - floor) / RANGE_DB).clamp(0.0, 1.0))
            .collect()
    }

    /// Añade el bloque al final del historial y descarta lo mas viejo.
    fn push_history(&mut self, block: &[f32]) {
        if block.len() >= FFT_SIZE {
            self.history
                .copy_from_slice(&block[block.len() - FFT_SIZE..]);
        } else {
            self.history.rotate_left(block.len());
            let start = FFT_SIZE - block.len();
            self.history[start..].copy_from_slice(block);
        }
    }

    fn is_silent(&self) -> bool {
        let mean_square: f32 = self.history.iter().map(|s| s * s).sum::<f32>() / FFT_SIZE as f32;
        mean_square < SILENCE_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = TARGET_RATE as f32;

    /// Banda que contiene una frecuencia, con la misma escala log que el analizador.
    fn band_of(hz: f32) -> usize {
        let x = (hz / LOW_HZ).ln() / (HIGH_HZ / LOW_HZ).ln() * BANDS as f32;
        (x.floor() as usize).min(BANDS - 1)
    }

    fn argmax(v: &[f32]) -> usize {
        v.iter()
            .enumerate()
            .fold(
                (0, f32::MIN),
                |best, (i, &x)| if x > best.1 { (i, x) } else { best },
            )
            .0
    }

    /// Bloques consecutivos de un seno, con la fase continua entre ellos.
    fn sine_blocks(hz: f32, amp: f32, block_len: usize, blocks: usize) -> Vec<Vec<f32>> {
        let mut n = 0usize;
        (0..blocks)
            .map(|_| {
                (0..block_len)
                    .map(|_| {
                        let t = n as f32 / RATE;
                        n += 1;
                        amp * (2.0 * std::f32::consts::PI * hz * t).sin()
                    })
                    .collect()
            })
            .collect()
    }

    /// Analiza los bloques y devuelve la salida del ultimo.
    fn run(spec: &mut Spectrum, blocks: &[Vec<f32>]) -> Vec<f32> {
        let mut out = Vec::new();
        for b in blocks {
            out = spec.analyze(b);
        }
        out
    }

    #[test]
    fn el_silencio_da_todas_las_bandas_a_cero() {
        let mut spec = Spectrum::new();
        let silence = vec![vec![0.0f32; 1600]; 5];
        let out = run(&mut spec, &silence);
        assert_eq!(out.len(), BANDS);
        assert!(out.iter().all(|&v| v < 0.05), "silencio: {out:?}");
    }

    #[test]
    fn un_seno_de_1khz_cae_en_su_banda_y_los_extremos_quedan_bajos() {
        let mut spec = Spectrum::new();
        let out = run(&mut spec, &sine_blocks(1000.0, 0.5, 1600, 5));
        let peak = band_of(1000.0);
        assert_eq!(argmax(&out), peak, "salida: {out:?}");
        assert!(out[peak] >= 0.8, "banda de 1 kHz: {}", out[peak]);
        assert!(out[0] <= 0.2, "banda mas grave: {}", out[0]);
        assert!(out[BANDS - 1] <= 0.2, "banda mas aguda: {}", out[BANDS - 1]);
    }

    #[test]
    fn un_seno_grave_pica_en_una_banda_mas_baja_que_uno_agudo() {
        let mut spec = Spectrum::new();
        let low = argmax(&run(&mut spec, &sine_blocks(200.0, 0.5, 1600, 5)));
        let mut spec = Spectrum::new();
        let high = argmax(&run(&mut spec, &sine_blocks(3000.0, 0.5, 1600, 5)));
        assert!(low < high, "200 Hz en banda {low}, 3 kHz en banda {high}");
    }

    #[test]
    fn la_salida_siempre_tiene_bands_valores_tambien_con_bloques_impares() {
        let mut spec = Spectrum::new();
        for block in sine_blocks(440.0, 0.3, 37, 50) {
            let out = spec.analyze(&block);
            assert_eq!(out.len(), BANDS);
            assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
        }
        assert_eq!(spec.analyze(&[]).len(), BANDS);
        assert_eq!(spec.analyze(&vec![0.1f32; 5000]).len(), BANDS);
    }
}
