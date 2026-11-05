//! Normalizacion de ganancia.
//!
//! Hace falta por un detalle medido, no teorico: **el loopback de WASAPI
//! captura despues del control de volumen**. Con un tono de origen a rms 0,636
//! y el deslizador de Windows bajo, lo que entra por el bucle es rms 0,0025:
//! 48 dB de atenuacion. El modelo recibiria practicamente silencio y no
//! transcribiria nada, y el usuario no tendria forma de saber por que.
//!
//! Asi que se reescala segun el pico reciente. Los cuidados que importan:
//!
//! - **No amplificar el silencio.** Por debajo de un suelo absoluto no se
//!   adapta nada, o el ruido de fondo acabaria a todo volumen entre frases.
//! - **No dar saltos.** La ganancia se mueve poco a poco; un cambio brusco
//!   mete un chasquido que el modelo interpreta como sonido.
//! - **Techo de ganancia.** Si de verdad no hay senal, subir x1000 solo
//!   amplifica ruido de cuantizacion.

/// Pico al que se intenta llevar la senal. Ni tan alto que recorte ni tan bajo
/// que el modelo trabaje al limite de su rango util.
const DEFAULT_TARGET_PEAK: f32 = 0.30;

/// Amplificacion maxima, ~+36 dB.
const DEFAULT_MAX_GAIN: f32 = 64.0;

/// Por debajo de esto (-80 dBFS) se considera que no hay senal que medir.
const DEFAULT_FLOOR: f32 = 0.0001;

/// Cuanto decae el pico rastreado en cada bloque. A 100 ms por bloque, 0,995
/// tarda unos 20 s en olvidar un pico, que es lo que queremos: seguir el nivel
/// general del audio, no cada silaba.
const DEFAULT_DECAY: f32 = 0.995;

/// Cuanto se acerca la ganancia a su objetivo en cada bloque.
const DEFAULT_SMOOTHING: f32 = 0.15;

#[derive(Debug, Clone)]
pub struct Normalizer {
    target_peak: f32,
    max_gain: f32,
    floor: f32,
    decay: f32,
    smoothing: f32,
    peak: f32,
    gain: f32,
    enabled: bool,
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Normalizer {
    pub fn new(enabled: bool) -> Self {
        Self {
            target_peak: DEFAULT_TARGET_PEAK,
            max_gain: DEFAULT_MAX_GAIN,
            floor: DEFAULT_FLOOR,
            decay: DEFAULT_DECAY,
            smoothing: DEFAULT_SMOOTHING,
            peak: 0.0,
            gain: 1.0,
            enabled,
        }
    }

    /// Ganancia aplicada ahora mismo. Util para mostrarla: si esta pegada al
    /// techo, es que el volumen del sistema esta demasiado bajo.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Pico rastreado del audio **antes** de normalizar.
    pub fn tracked_peak(&self) -> f32 {
        self.peak
    }

    /// Si la ganancia esta al maximo, ya no se puede compensar mas.
    pub fn at_ceiling(&self) -> bool {
        self.enabled && self.gain >= self.max_gain * 0.99
    }

    pub fn process(&mut self, block: &mut [f32]) {
        if !self.enabled || block.is_empty() {
            return;
        }

        let block_peak = block.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));

        // El pico decae siempre; solo sube si el bloque trae senal de verdad.
        self.peak *= self.decay;
        if block_peak > self.floor {
            self.peak = self.peak.max(block_peak);
        }

        // Sin senal medible se mantiene la ultima ganancia en vez de dispararla.
        let desired = if self.peak > self.floor {
            (self.target_peak / self.peak).clamp(1.0, self.max_gain)
        } else {
            self.gain
        };

        self.gain += (desired - self.gain) * self.smoothing;

        for sample in block.iter_mut() {
            // El limite duro es una red de seguridad: con el pico bien seguido
            // no deberia entrar casi nunca.
            *sample = (*sample * self.gain).clamp(-1.0, 1.0);
        }
    }
}

/// Valor eficaz del bloque. Se mide antes de normalizar: es el dato que dice
/// si el volumen del sistema esta demasiado bajo.
pub fn rms(block: &[f32]) -> f32 {
    if block.is_empty() {
        return 0.0;
    }
    let sum: f64 = block.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / block.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| if i % 2 == 0 { amp } else { -amp }).collect()
    }

    fn peak(block: &[f32]) -> f32 {
        block.iter().fold(0.0f32, |a, s| a.max(s.abs()))
    }

    #[test]
    fn desactivado_no_toca_nada() {
        let mut n = Normalizer::new(false);
        let mut block = tone(160, 0.001);
        let before = block.clone();
        n.process(&mut block);
        assert_eq!(block, before);
    }

    #[test]
    fn levanta_una_senal_muy_baja_hasta_cerca_del_objetivo() {
        let mut n = Normalizer::new(true);
        // El caso medido: loopback 48 dB por debajo.
        for _ in 0..300 {
            let mut block = tone(1600, 0.0025);
            n.process(&mut block);
        }
        let mut block = tone(1600, 0.0025);
        n.process(&mut block);
        let p = peak(&block);
        assert!(
            p > 0.15 && p <= 0.35,
            "deberia acercarse a 0.30, quedo en {p}"
        );
    }

    #[test]
    fn no_amplifica_el_silencio_digital() {
        let mut n = Normalizer::new(true);
        for _ in 0..500 {
            let mut block = vec![0.0f32; 1600];
            n.process(&mut block);
            assert!(block.iter().all(|s| *s == 0.0));
        }
        assert_eq!(n.gain(), 1.0, "sin senal no debe moverse la ganancia");
    }

    #[test]
    fn no_atenua_una_senal_que_ya_viene_fuerte() {
        let mut n = Normalizer::new(true);
        for _ in 0..200 {
            let mut block = tone(1600, 0.8);
            n.process(&mut block);
        }
        let mut block = tone(1600, 0.8);
        n.process(&mut block);
        // La ganancia esta acotada por abajo en 1.0: solo sube, nunca baja.
        assert!((peak(&block) - 0.8).abs() < 1e-6);
        assert_eq!(n.gain(), 1.0);
    }

    #[test]
    fn la_ganancia_sube_progresivamente_y_no_de_golpe() {
        let mut n = Normalizer::new(true);
        let mut block = tone(1600, 0.002);
        n.process(&mut block);
        let primera = n.gain();
        assert!(
            primera < 20.0,
            "un solo bloque no debe saltar a la ganancia final, fue {primera}"
        );
        for _ in 0..200 {
            let mut b = tone(1600, 0.002);
            n.process(&mut b);
        }
        assert!(n.gain() > primera, "con el tiempo si debe converger");
    }

    #[test]
    fn respeta_el_techo_de_ganancia() {
        let mut n = Normalizer::new(true);
        for _ in 0..2000 {
            let mut block = tone(1600, 0.0002);
            n.process(&mut block);
        }
        assert!(n.gain() <= DEFAULT_MAX_GAIN + 1e-3);
        assert!(n.at_ceiling(), "con senal asi de baja deberia avisar del techo");
    }
}
