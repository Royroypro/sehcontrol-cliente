import 'package:flutter/material.dart';
import 'package:get/get.dart';
import 'package:url_launcher/url_launcher.dart';

import '../../common.dart';
import '../../consts.dart';

class SupportSidebarBlock extends StatelessWidget {
  const SupportSidebarBlock({super.key});

  Future<void> _open(String url) async {
    final uri = Uri.parse(url);
    await launchUrl(uri, mode: LaunchMode.externalApplication);
  }

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.fromLTRB(10, 8, 10, 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Divider(height: 16),
          Text('Soporte',
              style: Theme.of(context)
                  .textTheme
                  .labelMedium
                  ?.copyWith(fontWeight: FontWeight.w600)),
          const SizedBox(height: 8),
          // Server-configured (UserModel.whatsappNumber, from
          // /api/client-policy's `whatsapp_number`) so the admin can change
          // the support number without a client release. Hidden entirely
          // rather than falling back to a hardcoded number when it isn't
          // configured — a wrong/stale number is worse than no button.
          Obx(() {
            final whatsapp = gFFI.userModel.whatsappNumber.value;
            if (whatsapp.isEmpty) return const SizedBox.shrink();
            return _MiniLink(
              icon: Icons.chat,
              iconColor: const Color(0xFF25D366),
              title: 'WhatsApp',
              subtitle: '+$whatsapp',
              onTap: () => _open('https://wa.me/$whatsapp'),
            );
          }),
          const SizedBox(height: 6),
          _MiniLink(
            icon: Icons.public,
            iconColor: MyTheme.accent,
            title: 'Web',
            subtitle: 'sehcontrol.sehuacho.com',
            onTap: () => _open(kSehcontrolWebsiteUrl),
          ),
        ],
      ),
    );
  }
}

class _MiniLink extends StatelessWidget {
  final IconData icon;
  final Color? iconColor;
  final String title;
  final String subtitle;
  final VoidCallback onTap;

  const _MiniLink({
    required this.icon,
    this.iconColor,
    required this.title,
    required this.subtitle,
    required this.onTap,
  });

  @override
  Widget build(BuildContext context) {
    return Material(
      color: Colors.transparent,
      child: InkWell(
        borderRadius: BorderRadius.circular(10),
        onTap: onTap,
        child: Padding(
          padding: const EdgeInsets.symmetric(vertical: 8, horizontal: 8),
          child: Row(
            children: [
              Icon(icon, size: 18, color: iconColor),
              const SizedBox(width: 10),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(title,
                        style: Theme.of(context)
                            .textTheme
                            .bodyMedium
                            ?.copyWith(fontWeight: FontWeight.w600)),
                    const SizedBox(height: 2),
                    Text(subtitle,
                        style: Theme.of(context).textTheme.bodySmall),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
