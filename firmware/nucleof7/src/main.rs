#![feature(asm)]
#![feature(used)]
#![no_std]

#[macro_use]
extern crate cortex_m;
extern crate cortex_m_rt;
extern crate stm32f7;
extern crate smoltcp;

use core::u16;
use cortex_m::asm;
use stm32f7::stm32f7x7::{GPIOA, GPIOB, GPIOC, GPIOD, GPIOG, USART3, TIM5};
use stm32f7::stm32f7x7::{RCC, PWR, SYSCFG, FLASH, ETHERNET_MAC, ETHERNET_DMA};
use smoltcp::phy::Device;
use smoltcp::iface::{ArpCache, SliceArpCache, EthernetInterface};
use smoltcp::socket::{AsSocket, SocketSet};
use smoltcp::socket::{TcpSocket, TcpSocketBuffer};
use smoltcp::wire::{EthernetAddress, IpAddress};

static mut TBUF: [[u32; 512]; 2] = [[0; 512]; 2];
static mut RBUF: [[u32; 512]; 2] = [[0; 512]; 2];

#[repr(C,packed)]
#[derive(Copy,Clone)]
struct TDes {
    tdes0: u32,
    tdes1: u32,
    tdes2: u32,
    tdes3: u32,
}

static mut TD0: TDes = TDes { tdes0: 0, tdes1: 0, tdes2: 0, tdes3: 0 };
static mut TD1: TDes = TDes { tdes0: 0, tdes1: 0, tdes2: 0, tdes3: 0 };
static mut TDPTR: &TDes = unsafe { &TD0 };

#[repr(C,packed)]
#[derive(Copy,Clone)]
struct RDes {
    rdes0: u32,
    rdes1: u32,
    rdes2: u32,
    rdes3: u32,
}

static mut RD0: RDes = RDes { rdes0: 0, rdes1: 0, rdes2: 0, rdes3: 0 };
static mut RD1: RDes = RDes { rdes0: 0, rdes1: 0, rdes2: 0, rdes3: 0 };
static mut RDPTR: &RDes = unsafe { &RD0 };

fn rxne() -> bool {
    unsafe {
        RDPTR = &*(RDPTR.rdes3 as *const RDes);
        asm::dmb();
        RDPTR.rdes0 & (1<<31) == 0
    }
}

fn txe() -> bool {
    unsafe {
        TDPTR = &*(TDPTR.tdes3 as *const TDes);
        asm::dmb();
        TDPTR.tdes0 & (1<<31) == 0
    }
}

fn transmit_packet(packet: &[u8]) {
    unsafe {
        let len = packet.len();

        if packet as *const _ as *const u8 == &(TBUF[0]) as *const _ as *const u8 {
            TD0.tdes1 = len as u32;
            TD0.tdes0 = (1<<31) | (1<<30) | (1<<29) | (1<<28) | (3<<22) | (1<<20);
        } else {
            TD1.tdes1 = len as u32;
            TD1.tdes0 = (1<<31) | (1<<30) | (1<<29) | (1<<28) | (3<<22) | (1<<20);
        }
        asm::dmb();

        /* If DMA has stopped, tell it to restart */
        cortex_m::interrupt::free(|cs| {
            let eth_dma = ETHERNET_DMA.borrow(cs);
            if eth_dma.dmasr.read().tps().bits() == 0b110 {
                eth_dma.dmasr.write(|w| w.tbus().set_bit());
                eth_dma.dmatpdr.write(|w| w.tpd().bits(0xFFFF_FFFF));
            }
        });
    }
}

fn get_tx_buffer(length: usize) -> &'static mut [u8] {
    unsafe {
        core::slice::from_raw_parts_mut(TDPTR.tdes2 as *mut u8, length)
    }
}

fn receive_packet() -> &'static [u8] {
    unsafe {
        let len = (RDPTR.rdes0 >> 16) & 0x3FFF;
        core::slice::from_raw_parts(RDPTR.rdes2 as *const u8, len as usize)
    }
}

fn release_packet(buf: &[u8]) {
    unsafe {
        if buf as *const _ as *const u8 == &(RBUF[0]) as *const _ as *const u8 {
            RD0.rdes0 = 1<<31;
        } else {
            RD1.rdes0 = 1<<31;
        }
        asm::dmb();

        // Check if DMA has stopped and restart if so
        cortex_m::interrupt::free(|cs| {
            let eth_dma = ETHERNET_DMA.borrow(cs);
            if eth_dma.dmasr.read().rps().bits() == 0b110 {
                eth_dma.dmasr.write(|w| w.rbus().set_bit());
                eth_dma.dmarpdr.write(|w| w.rpd().bits(0xFFFF_FFFF));
            }
        });
    }
}

struct EthernetRxBuffer(&'static [u8]);
struct EthernetTxBuffer(&'static mut [u8]);

impl AsRef<[u8]> for EthernetTxBuffer { fn as_ref(&self) -> &[u8] { self.0 } }
impl AsRef<[u8]> for EthernetRxBuffer { fn as_ref(&self) -> &[u8] { self.0 } }
impl AsMut<[u8]> for EthernetTxBuffer { fn as_mut(&mut self) -> &mut [u8] { self.0 } }

impl Drop for EthernetTxBuffer {
    fn drop(&mut self) {
        transmit_packet(self.0);
    }
}

impl Drop for EthernetRxBuffer {
    fn drop(&mut self) {
        release_packet(self.0);
    }
}

struct EthernetDevice {}

impl Device for EthernetDevice {
    type RxBuffer = EthernetRxBuffer;
    type TxBuffer = EthernetTxBuffer;

    fn mtu(&self) -> usize { 1536 }

    fn receive(&mut self) -> Result<Self::RxBuffer, smoltcp::Error> {
        if rxne() {
            Ok(EthernetRxBuffer(receive_packet()))
        } else {
            Err(smoltcp::Error::Exhausted)
        }
    }

    fn transmit(&mut self, length: usize) -> Result<Self::TxBuffer, smoltcp::Error> {
        if txe() {
            Ok(EthernetTxBuffer(get_tx_buffer(length)))
        } else {
            Err(smoltcp::Error::Exhausted)
        }
    }
}

fn gpio_init() {
    cortex_m::interrupt::free(|cs| {
        let gpioa = GPIOA.borrow(cs);
        let gpiob = GPIOB.borrow(cs);
        let gpioc = GPIOC.borrow(cs);
        let gpiod = GPIOD.borrow(cs);
        let gpiog = GPIOG.borrow(cs);

        // GPIOA 1, 2, 7
        // GPIOB 13
        // GPIOC 1, 4, 5
        // GPIOG 11, 13
        //
        // All set to alternate function 11 and very high speed.

        gpioa.moder.modify(|_, w|
            w.moder1().alternate()
             .moder2().alternate()
             .moder7().alternate());

        gpiob.moder.modify(|_, w|
             w.moder13().alternate());

        gpioc.moder.modify(|_, w|
            w.moder1().alternate()
             .moder4().alternate()
             .moder5().alternate());

        gpiod.moder.modify(|_, w|
            w.moder8().alternate()
             .moder9().alternate());

        gpiog.moder.modify(|_, w|
            w.moder11().alternate()
             .moder13().alternate());

        gpioa.ospeedr.modify(|_, w|
            w.ospeedr1().very_high_speed()
             .ospeedr2().very_high_speed()
             .ospeedr7().very_high_speed());

        gpiob.ospeedr.modify(|_, w|
            w.ospeedr13().very_high_speed());

        gpioc.ospeedr.modify(|_, w|
            w.ospeedr1().very_high_speed()
             .ospeedr4().very_high_speed()
             .ospeedr5().very_high_speed());

        gpiog.ospeedr.modify(|_, w|
            w.ospeedr11().very_high_speed()
             .ospeedr13().very_high_speed());

        gpioa.afrl.modify(|_, w|
            w.afrl1().af11()
             .afrl2().af11()
             .afrl7().af11());

        gpiob.afrh.modify(|_, w|
            w.afrh13().af11());

        gpioc.afrl.modify(|_, w|
            w.afrl1().af11()
             .afrl4().af11()
             .afrl5().af11());

        gpiod.afrh.modify(|_, w|
            w.afrh8().af7()
             .afrh9().af7());

        gpiog.afrh.modify(|_, w|
            w.afrh11().af11()
             .afrh13().af11());
    });
}

fn rcc_init() {
    cortex_m::interrupt::free(|cs| {
        let rcc = RCC.borrow(cs);
        let pwr = PWR.borrow(cs);
        let flash = FLASH.borrow(cs);

        rcc.apb1enr.modify(|_, w| w.pwren().enabled());

        // VOS_SCALE2
        unsafe { pwr.cr1.write(|w| w.bits(0x00008000)) }

        // Ensure HSI is on and stable
        rcc.cr.modify(|_, w| w.hsion().set_bit());
        while rcc.cr.read().hsion().is_bit_clear() {}

        // Set to HSI
        rcc.cfgr.modify(|_, w| w.sw().hsi());
        while !rcc.cfgr.read().sws().is_hsi() {}

        // Clear register to reset value
        rcc.cr.write(|w| w.hsion().set_bit());
        rcc.cfgr.write(|w| unsafe { w.bits(0) });

        // Activate PLL
        rcc.pllcfgr.write(|w| unsafe { w.pllq().bits(4).pllsrc().hsi().pllp().div2().plln().bits(168).pllm().bits(8) });
        rcc.cr.modify(|_, w| w.pllon().set_bit());

        // Other clock settings
        rcc.cfgr.modify(|_, w| w.ppre2().div2().ppre1().div4().hpre().div1());

        // TODO DCKFGR2

        // Flash setup
        flash.acr.write(|w| unsafe { w.arten().set_bit().prften().set_bit().latency().bits(5) });

        // Swap to PLL
        rcc.cfgr.modify(|_, w| w.sw().pll());
        while !rcc.cfgr.read().sws().is_pll() {}

    });
}

fn rcc_reset() {
    // Reset GPIOs and ethernet, and set ethernet to RMII
    cortex_m::interrupt::free(|cs| {
        let rcc = RCC.borrow(cs);
        rcc.ahb1rstr.write(|w|
            w.gpioarst().reset()
             .gpiobrst().reset()
             .gpiocrst().reset()
             .gpiodrst().reset()
             .gpiogrst().reset()
             .ethmacrst().reset()
        );
        rcc.apb1rstr.write(|w|
            w.uart3rst().reset()
             .tim3rst().reset()
        );
        rcc.ahb1rstr.write(|w| unsafe { w.bits(0)});
        rcc.apb1rstr.write(|w| unsafe { w.bits(0)});
    });
}

fn rcc_start() {
    // Enable GPIO and ethernet clocks
    cortex_m::interrupt::free(|cs| {
        let rcc = RCC.borrow(cs);
        rcc.ahb1enr.modify(|_, w|
            w.gpioaen().enabled()
             .gpioben().enabled()
             .gpiocen().enabled()
             .gpioden().enabled()
             .gpiogen().enabled()
             .ethmacrxen().enabled()
             .ethmactxen().enabled()
             .ethmacen().enabled()
             .dma1en().enabled()
             .dma2en().enabled()
        );
        rcc.apb2enr.modify(|_, w|
            w.syscfgen().enabled()
        );
        rcc.apb1enr.modify(|_, w|
            w.usart3en().enabled()
             .tim5en().enabled()
        );
    });
}

fn usart_init() {
    cortex_m::interrupt::free(|cs| {
        let usart3 = USART3.borrow(cs);
        usart3.cr1.modify(|_, w|
            w.te().set_bit()
             .re().set_bit()
        );
        usart3.cr3.modify(|_, w|
            w.dmat().set_bit()
        );
        usart3.brr.write(|w| unsafe { w.bits(4375) });
        usart3.cr1.modify(|_, w| w.ue().set_bit());
    });
}

fn usart_send(data: u32) {
    cortex_m::interrupt::free(|cs| {
        let usart3 = USART3.borrow(cs);
        while usart3.isr.read().txe().is_bit_clear() {}
        usart3.tdr.write(|w| unsafe { w.bits(data) });
    });
}

fn usart_send_string(data: &str) {
    for c in data.as_bytes() {
        usart_send(*c as u32);
    }
}

fn timer_init() {
    cortex_m::interrupt::free(|cs| {
        let tim5 = TIM5.borrow(cs);
        tim5.psc.write(|w| unsafe { w.psc().bits(42000 - 1) });
        tim5.arr.write(|w| unsafe {
            w.arr_l().bits(0xffff).arr_h().bits(0xffff) });
        tim5.cr1.modify(|_, w| w.cen().set_bit());
    });
}

fn timer_time() -> u32 {
    cortex_m::interrupt::free(|cs| {
        let tim5 = TIM5.borrow(cs);
        let low = tim5.cnt.read().cnt_l().bits() as u32;
        let high = tim5.cnt.read().cnt_h().bits() as u32;
        high<<16 | low
    })
}

fn mac_init() {
    cortex_m::interrupt::free(|cs| {
        let syscfg = SYSCFG.borrow(cs);
        let eth_mac = ETHERNET_MAC.borrow(cs);
        let eth_dma = ETHERNET_DMA.borrow(cs);

        // Set to RMII mode
        syscfg.pmc.modify(|_, w| w.mii_rmii_sel().set_bit());

        // Reset ETH DMA
        eth_dma.dmabmr.modify(|_, w| w.sr().set_bit());
        while eth_dma.dmabmr.read().sr().is_bit_set() {}

        // Set MAC address 56:54:9f:08:87:1d
        eth_mac.maca0lr.write(|w| unsafe { w.bits(0x089f5456) });
        eth_mac.maca0hr.write(|w| unsafe { w.bits(0x1d87) });

        // Receive all
        eth_mac.macffr.modify(|_, w| w.ra().set_bit().pm().set_bit());

        // Enable RX and TX now. Set link speed and duplex at link-up event.
        eth_mac.maccr.write(|w|
            w.re().set_bit()
             .te().set_bit());

        // Set up DMA descriptors in chain mode
        unsafe {
            TD0.tdes0 = 1<<20;
            TD0.tdes1 = 2048;
            TD0.tdes2 = (&TBUF[0] as *const u32) as u32;
            TD0.tdes3 = (&TD1   as *const TDes) as u32;

            TD1.tdes0 = 1<<20;
            TD1.tdes1 = 2048;
            TD1.tdes2 = (&TBUF[1] as *const u32) as u32;
            TD1.tdes3 = (&TD0   as *const TDes) as u32;

            TDPTR = &TD0;

            RD0.rdes0 = 1<<31;
            RD0.rdes1 = 2048 | (1<<14);
            RD0.rdes2 = (&RBUF[0] as *const u32) as u32;
            RD0.rdes3 = (&RD1   as *const RDes) as u32;

            RD1.rdes0 = 1<<31;
            RD1.rdes1 = 2048 | (1<<14);
            RD1.rdes2 = (&RBUF[1] as *const u32) as u32;
            RD1.rdes3 = (&RD0   as *const RDes) as u32;

            RDPTR = &RD0;
        }

        eth_dma.dmatdlar.write(|w| unsafe { w.bits((&TD0 as *const TDes) as u32 )});
        eth_dma.dmardlar.write(|w| unsafe { w.bits((&RD0 as *const RDes) as u32 )});

        // Set DMA interrupts
        eth_dma.dmaier.write(|w|
            w.nise().set_bit()
             .rie().set_bit()
             .tie().set_bit());

        // Set DMA bus mode
        eth_dma.dmabmr.write(|w| unsafe {
            w.aab().set_bit()
             .pbl().bits(1)
             .sr().clear_bit()
        });

        // Flush TX FIFO
        eth_dma.dmaomr.write(|w| w.ftf().set_bit());
        while eth_dma.dmaomr.read().ftf().is_bit_set() {}

        // Set operation mode and start DMA
        eth_dma.dmaomr.write(|w|
            w.rsf().set_bit()
             .tsf().set_bit()
             .st().set_bit()
             .sr().set_bit());
    });
}

fn smi_read(reg: u8) -> u16 {
    let mut result = 0;

    cortex_m::interrupt::free(|cs| {
        let eth_mac = ETHERNET_MAC.borrow(cs);

        // Use PHY address 00000, set register address,
        // set clock to HCLK/102, start read operation
        eth_mac.macmiiar.write(|w| unsafe {
            w.mb().set_bit()
             .cr().bits(4)
             .mr().bits(reg)
        });

        // Wait for read to complete
        while eth_mac.macmiiar.read().mb().is_bit_set() {}

        // Return resulting data
        result = eth_mac.macmiidr.read().td().bits();
    });

    result
}

fn smi_write(reg: u8, val: u16) {
    cortex_m::interrupt::free(|cs| {
        let eth_mac = ETHERNET_MAC.borrow(cs);

        eth_mac.macmiidr.write(|w| unsafe { w.td().bits(val) });
        eth_mac.macmiiar.write(|w| unsafe {
            w.mb().set_bit()
             .mw().set_bit()
             .cr().bits(4)
             .mr().bits(reg)
        });

        while eth_mac.macmiiar.read().mb().is_bit_set() {}
    });
}

fn phy_reset() {
    smi_write(0x00, (1<<15));
    while smi_read(0x00) & (1<<15) == (1<<15) {}
}

fn phy_init() {
    smi_write(0x00, (1<<12));
}

fn phy_poll_link() -> bool {
    let bsr = smi_read(0x01);
    let bcr = smi_read(0x00);
    let lpa = smi_read(0x05);

    // No link if no auto negotiate
    if bcr & (1<<12) == 0 { return false; }
    // No link if link is down
    if bsr & (1<< 2) == 0 { return false; }
    // No link if remote fault
    if bsr & (1<< 4) != 0 { return false; }
    // No link if autonegotiate incomplete
    if bsr & (1<< 5) == 0 { return false; }
    // No link if other side can't do 100Mbps full duplex
    if lpa & (1<< 8) == 0 { return false; }

    // Otherwise we have link
    cortex_m::interrupt::free(|cs| {
        let eth_mac = ETHERNET_MAC.borrow(cs);
        eth_mac.maccr.modify(|_, w| w.fes().set_bit().dm().set_bit());
    });

    true
}

#[inline(never)]
fn main() {
    cortex_m::interrupt::free(|cs| {
        let scb = cortex_m::peripheral::SCB.borrow(cs);
        let cpuid = cortex_m::peripheral::CPUID.borrow(cs);
        //scb.enable_dcache(&cpuid);
        //scb.enable_icache();
        scb.disable_dcache(&cpuid);
    });

    rcc_init();
    rcc_start();
    rcc_reset();
    gpio_init();
    timer_init();

    usart_init();
    usart_send_string("\r\n\r\nInitialising network...\r\n");

    phy_reset();
    phy_init();
    mac_init();

    let mut arpcache_storage = [Default::default(); 8];
    let mut arpcache = SliceArpCache::new(&mut arpcache_storage[..]);
    let mut ethdev = EthernetDevice {};
    let ethaddr = EthernetAddress::from_bytes(&[0x56, 0x54, 0x9f, 0x08, 0x87, 0x1d]);
    let mut ipaddrs = [IpAddress::v4(192, 168, 2, 202)];
    let mut ethiface = EthernetInterface::new(
        &mut ethdev, &mut arpcache as &mut ArpCache, ethaddr, &mut ipaddrs[..]);

    let mut tcp_rx_buf_storage = [Default::default(); 128];
    let mut tcp_tx_buf_storage = [Default::default(); 128];
    let tcp_rx_buf = TcpSocketBuffer::new(&mut tcp_rx_buf_storage[..]);
    let tcp_tx_buf = TcpSocketBuffer::new(&mut tcp_tx_buf_storage[..]);
    let tcp_socket = TcpSocket::new(tcp_rx_buf, tcp_tx_buf);

    let mut sockets_storage = [Default::default(), Default::default()];
    let mut sockets = SocketSet::new(&mut sockets_storage[..]);
    let tcp_handle = sockets.add(tcp_socket);

    usart_send_string("Waiting for link...\r\n");
    while !phy_poll_link() { }
    usart_send_string("Link established.\r\n");

    loop {
        {
            let socket: &mut TcpSocket = sockets.get_mut(tcp_handle).as_socket();
            if !socket.is_open() {
                socket.listen(6969).unwrap();
            }
            if socket.can_send() {
                let data = b"hello world\n";
                usart_send_string("Sending TCP data\r\n");
                socket.send_slice(data).unwrap();
                usart_send_string("Closing socket\r\n");
                socket.close();
            }
        }

        let time_ms = timer_time() as u64;
        match ethiface.poll(&mut sockets, time_ms) {
            Ok(()) | Err(smoltcp::Error::Exhausted) => (),
            Err(_) => usart_send_string("Network error\r\n"),
        }
    }
}

#[allow(dead_code)]
#[used]
#[link_section = ".rodata.interrupts"]
static INTERRUPTS: [extern "C" fn(); 240] = [default_handler; 240];

extern "C" fn default_handler() {
    asm::bkpt();
}
