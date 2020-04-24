use std::io::{BufRead};
use std::iter::{Peekable, Iterator};
use std::collections::HashSet;

use anyhow::{Error, bail, format_err};
use lazy_static::lazy_static;
use regex::Regex;

use super::helper::*;
use super::lexer::*;

use super::{NetworkConfig, NetworkOrderEntry, Interface, NetworkConfigMethod, NetworkInterfaceType};

pub struct NetworkParser<R: BufRead> {
    input: Peekable<Lexer<R>>,
    line_nr: usize,
}

impl <R: BufRead> NetworkParser<R> {

    pub fn new(reader: R) -> Self {
        let input = Lexer::new(reader).peekable();
        Self { input, line_nr: 1 }
    }

    fn peek(&mut self) -> Result<Token, Error> {
        match self.input.peek() {
            Some(Err(err)) => {
                bail!("input error - {}", err);
            }
            Some(Ok((token, _))) => {
                return Ok(*token);
            }
            None => {
                bail!("got unexpected end of stream (inside peek)");
            }
        }
    }

    fn next(&mut self) -> Result<(Token, String), Error> {
        match self.input.next() {
            Some(Err(err)) => {
                bail!("input error - {}", err);
            }
            Some(Ok((token, text))) => {
                if token == Token::Newline { self.line_nr += 1; }
                return Ok((token, text));
            }
            None => {
                bail!("got unexpected end of stream (inside peek)");
            }
        }
    }

    fn next_text(&mut self) -> Result<String, Error> {
        match self.next()? {
            (Token::Text, text) => Ok(text),
            (unexpected, _) => bail!("got unexpected token {:?} (expecting Text)", unexpected),
        }
    }

    fn eat(&mut self, expected: Token) -> Result<String, Error> {
        let (next, text) = self.next()?;
        if next != expected {
            bail!("expected {:?}, got {:?}", expected, next);
        }
        Ok(text)
    }

    fn parse_auto(&mut self, auto_flag: &mut HashSet<String>) -> Result<(), Error> {
        self.eat(Token::Auto)?;

        loop {
            match self.next()? {
                (Token::Text, iface) => {
                     auto_flag.insert(iface.to_string());
                }
                (Token::Newline, _) => break,
                unexpected => {
                    bail!("expected {:?}, got {:?}", Token::Text, unexpected);
                }
            }

        }

        Ok(())
    }

    fn parse_iface_address(&mut self, interface: &mut Interface) -> Result<(), Error> {
        self.eat(Token::Address)?;
        let cidr = self.next_text()?;

        let (_address, _mask, ipv6) = parse_cidr(&cidr)?;
        if ipv6 {
            interface.set_cidr_v6(cidr)?;
        } else {
            interface.set_cidr_v4(cidr)?;
        }

        self.eat(Token::Newline)?;

        Ok(())
    }

    fn parse_iface_gateway(&mut self, interface: &mut Interface) -> Result<(), Error> {
        self.eat(Token::Gateway)?;
        let gateway = self.next_text()?;

        if proxmox::tools::common_regex::IP_REGEX.is_match(&gateway) {
            if gateway.contains(':') {
                interface.set_gateway_v6(gateway)?;
            } else {
                interface.set_gateway_v4(gateway)?;
            }
        } else {
            bail!("unable to parse gateway address");
        }

        self.eat(Token::Newline)?;

        Ok(())
    }

    fn parse_iface_mtu(&mut self) -> Result<u64, Error> {
        self.eat(Token::MTU)?;

        let mtu = self.next_text()?;
        let mtu = match u64::from_str_radix(&mtu, 10) {
            Ok(mtu) => mtu,
            Err(err) => {
                bail!("unable to parse mtu value '{}' - {}", mtu, err);
            }
        };

        self.eat(Token::Newline)?;

        Ok(mtu)
    }

    fn parse_to_eol(&mut self) -> Result<String, Error> {
        let mut line = String::new();
        loop {
            match self.next()? {
                (Token::Newline, _) => return Ok(line),
                (_, text) => {
                    if !line.is_empty() { line.push(' '); }
                    line.push_str(&text);
                }
            }
        }
    }

    fn parse_iface_list(&mut self) -> Result<Vec<String>, Error> {
        let mut list = Vec::new();

        loop {
            let (token, text) = self.next()?;
            match token {
                Token::Newline => break,
                Token::Text => {
                    if &text != "none" {
                        list.push(text);
                    }
                }
                _ => bail!("unable to parse interface list - unexpected token '{:?}'", token),
            }
        }

        Ok(list)
    }

    fn parse_iface_attributes(
        &mut self,
        interface: &mut Interface,
        address_family_v4: bool,
        address_family_v6: bool,
    ) -> Result<(), Error> {

        loop {
            match self.peek()? {
                Token::Attribute => { self.eat(Token::Attribute)?; },
                Token::Comment => {
                    let comment = self.eat(Token::Comment)?;
                    if !address_family_v4 && address_family_v6 {
                        interface.comments_v6.push(comment);
                    } else {
                        interface.comments_v4.push(comment);
                    }
                    self.eat(Token::Newline)?;
                    continue;
                }
                Token::Newline => break,
                Token::EOF => break,
                unexpected => bail!("unexpected token {:?} (expected iface attribute)", unexpected),
            }

            match self.peek()? {
                Token::Address => self.parse_iface_address(interface)?,
                Token::Gateway => self.parse_iface_gateway(interface)?,
                Token::MTU => {
                    let mtu = self.parse_iface_mtu()?;
                    interface.mtu = Some(mtu);
                }
                Token::BridgePorts => {
                    self.eat(Token::BridgePorts)?;
                    let ports = self.parse_iface_list()?;
                    interface.bridge_ports = Some(ports);
                    interface.set_interface_type(NetworkInterfaceType::Bridge)?;
                }
                Token::BondSlaves => {
                    self.eat(Token::BondSlaves)?;
                    let slaves = self.parse_iface_list()?;
                    interface.bond_slaves = Some(slaves);
                    interface.set_interface_type(NetworkInterfaceType::Bond)?;
                }
                Token::Netmask => bail!("netmask is deprecated and no longer supported"),

                _ => { // parse addon attributes
                    let option = self.parse_to_eol()?;
                    if !option.is_empty() {
                        if !address_family_v4 && address_family_v6 {
                            interface.options_v6.push(option);
                        } else {
                            interface.options_v4.push(option);
                        }
                   };
                 },
            }
        }

        Ok(())
    }

    fn parse_iface(&mut self, config: &mut NetworkConfig) -> Result<(), Error> {
        self.eat(Token::Iface)?;
        let iface = self.next_text()?;

        let mut address_family_v4 = false;
        let mut address_family_v6 = false;
        let mut config_method = None;

        loop {
            let (token, text) = self.next()?;
            match token {
                Token::Newline => break,
                Token::Inet => address_family_v4 = true,
                Token::Inet6 => address_family_v6 = true,
                Token::Loopback => config_method = Some(NetworkConfigMethod::Loopback),
                Token::Static => config_method = Some(NetworkConfigMethod::Static),
                Token::Manual => config_method = Some(NetworkConfigMethod::Manual),
                Token::DHCP => config_method = Some(NetworkConfigMethod::DHCP),
                _ => bail!("unknown iface option {}", text),
            }
        }

        let config_method = config_method.unwrap_or(NetworkConfigMethod::Static);

        if !(address_family_v4 || address_family_v6) {
            address_family_v4 = true;
            address_family_v6 = true;
        }

        if let Some(mut interface) = config.interfaces.get_mut(&iface) {
            if address_family_v4 {
                interface.set_method_v4(config_method)?;
            }
            if address_family_v6 {
                interface.set_method_v6(config_method)?;
            }

            self.parse_iface_attributes(&mut interface, address_family_v4, address_family_v6)?;
        } else {
            let mut interface = Interface::new(iface.clone());
            if address_family_v4 {
                interface.set_method_v4(config_method)?;
            }
            if address_family_v6 {
                interface.set_method_v6(config_method)?;
            }

            self.parse_iface_attributes(&mut interface, address_family_v4, address_family_v6)?;

            config.interfaces.insert(interface.name.clone(), interface);

            config.order.push(NetworkOrderEntry::Iface(iface));
        }

        Ok(())
    }

    pub fn parse_interfaces(&mut self) -> Result<NetworkConfig, Error> {
        self._parse_interfaces()
            .map_err(|err| format_err!("line {}: {}", self.line_nr, err))
    }

    pub fn _parse_interfaces(&mut self) -> Result<NetworkConfig, Error> {
        let mut config = NetworkConfig::new();

        let mut auto_flag: HashSet<String> = HashSet::new();

        loop {
            match self.peek()? {
                Token::EOF => {
                    break;
                }
                Token::Newline => {
                    // skip empty lines
                    self.eat(Token::Newline)?;
                }
                Token::Comment => {
                    let (_, text) = self.next()?;
                    config.order.push(NetworkOrderEntry::Comment(text));
                    self.eat(Token::Newline)?;
                }
                Token::Auto => {
                    self.parse_auto(&mut auto_flag)?;
                }
                Token::Iface => {
                    self.parse_iface(&mut config)?;
                }
                _ => {
                    let option = self.parse_to_eol()?;
                    if !option.is_empty() {
                        config.order.push(NetworkOrderEntry::Option(option));
                    }
                }
            }
        }

        for iface in auto_flag.iter() {
            if let Some(interface) = config.interfaces.get_mut(iface) {
                interface.auto = true;
            }
        }

        let existing_interfaces = get_network_interfaces()?;

        lazy_static!{
            static ref PHYSICAL_NIC_REGEX: Regex = Regex::new(r"^(?:eth\d+|en[^:.]+|ib\d+)$").unwrap();
            static ref INTERFACE_ALIAS_REGEX: Regex = Regex::new(r"^\S+:\d+$").unwrap();
            static ref VLAN_INTERFACE_REGEX: Regex = Regex::new(r"^\S+\.\d+$").unwrap();
        }

        for (iface, active) in existing_interfaces.iter()  {
            if let Some(interface) = config.interfaces.get_mut(iface) {
                interface.active = *active;
                if interface.interface_type == NetworkInterfaceType::Unknown {
                    interface.interface_type = NetworkInterfaceType::Ethernet;
                }
           } else if PHYSICAL_NIC_REGEX.is_match(iface) { // also add all physical NICs
                let mut interface = Interface::new(iface.clone());
                interface.set_method_v4(NetworkConfigMethod::Manual)?;
                interface.interface_type = NetworkInterfaceType::Ethernet;
                interface.active = *active;
                config.interfaces.insert(interface.name.clone(), interface);
                config.order.push(NetworkOrderEntry::Iface(iface.to_string()));
            }
        }

        for (name, interface) in config.interfaces.iter_mut() {
            if interface.interface_type != NetworkInterfaceType::Unknown { continue; }
            if name == "lo" {
                interface.interface_type = NetworkInterfaceType::Loopback;
                continue;
            }
            if INTERFACE_ALIAS_REGEX.is_match(name) {
                interface.interface_type = NetworkInterfaceType::Alias;
                continue;
            }
            if VLAN_INTERFACE_REGEX.is_match(name) {
                interface.interface_type = NetworkInterfaceType::Vlan;
                continue;
            }
            if PHYSICAL_NIC_REGEX.is_match(name) {
                interface.interface_type = NetworkInterfaceType::Vanished;
                continue;
            }
        }

        Ok(config)
    }
}
